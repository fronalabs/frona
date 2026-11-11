use std::collections::{HashMap, HashSet};

use frona_text::{GroundingError as TextGroundingError, GroundingText};

use crate::evidence::{ExecutionEvidenceClass, ToolEvidenceProjection, ToolSupportCitation};
use crate::ingest::correction::{GroundingFailure, claim_clauses};
use crate::ingest::submission::{Batch, CandidateAttribute, SourceCitation, ToolEvidenceCitation};
use crate::model::{EvidenceSource, EvidenceStrength, MemoryEvidence, TemporalAnchor};
use crate::path::normalize_path;
use crate::{TranscriptEvidenceKind, TranscriptEvidenceSource};

pub(super) fn inherit_attribute_tool_support(batch: &mut Batch, failures: &[GroundingFailure]) {
    let supported = batch
        .memories
        .iter()
        .enumerate()
        .filter(|(index, memory)| {
            !failures.iter().any(|failure| {
                failure
                    .field_path
                    .starts_with(&format!("memories[{index}]"))
            }) && !memory.tool_evidence.is_empty()
        })
        .map(|(_, memory)| memory.clone())
        .collect::<Vec<_>>();
    let inherit = |path: &str, attribute: &mut CandidateAttribute| {
        if !attribute.tool_evidence.is_empty() {
            return;
        }
        let Some(memory) = supported.iter().find(|memory| {
            memory
                .entities
                .iter()
                .filter_map(|entity| normalize_path(entity))
                .any(|entity| entity == path)
                && GroundingText::new(&memory.content)
                    .resolve_value(&attribute.value)
                    .is_ok()
        }) else {
            return;
        };
        attribute.tool_evidence = memory.tool_evidence.clone();
    };
    for entity in &mut batch.new_entities {
        if let Some(path) = normalize_path(&entity.path) {
            for attribute in &mut entity.candidate_attributes {
                inherit(&path, attribute);
            }
        }
    }
    for entity in &mut batch.existing_entity_updates {
        if let Some(path) = normalize_path(&entity.path) {
            for attribute in &mut entity.candidate_attributes {
                inherit(&path, attribute);
            }
        }
    }
}

pub(super) fn validate_agent_tool_grounding(
    field_path: &str,
    claim: &str,
    citations: &mut [SourceCitation],
    tool_citations: &mut Vec<ToolEvidenceCitation>,
    sources: &[TranscriptEvidenceSource],
    evidence: &ToolEvidenceProjection,
    failures: &mut Vec<GroundingFailure>,
) {
    let assertions = citations
        .iter()
        .filter_map(|citation| {
            let source = sources
                .iter()
                .find(|source| source.handle == citation.message)?;
            let TranscriptEvidenceKind::AgentMessage { message_id, .. } = &source.kind else {
                return None;
            };
            Some((
                citation.message.clone(),
                message_id.clone(),
                citation.quote.clone(),
                citation.strength,
            ))
        })
        .collect::<Vec<_>>();
    if assertions.is_empty() && tool_citations.is_empty() {
        return;
    }
    let bypass = citations.iter().any(|citation| citation.confirmation)
        || citations.iter().any(|citation| {
            sources.iter().any(|source| {
                source.handle == citation.message
                    && matches!(
                        source.kind,
                        TranscriptEvidenceKind::UserMessage { .. }
                            | TranscriptEvidenceKind::TaskLifecycle { .. }
                    )
            })
        });

    if !tool_citations.is_empty() {
        for citation in tool_citations.iter() {
            let Some((handle, message_id, _, _)) = assertions
                .iter()
                .find(|(handle, _, _, _)| handle == &citation.message)
            else {
                failures.push(GroundingFailure {
                    field_path: format!("{field_path}.tool_evidence"),
                    message: citation.message.clone(),
                    submitted: citation.evidence_id.clone(),
                    reason: "tool_evidence_without_agent_source",
                });
                continue;
            };
            let Some(selected) =
                evidence.resolve_evidence_id(&citation.message, message_id, &citation.evidence_id)
            else {
                failures.push(GroundingFailure {
                    field_path: format!("{field_path}.tool_evidence"),
                    message: handle.clone(),
                    submitted: citation.evidence_id.clone(),
                    reason: "unknown_tool_evidence",
                });
                continue;
            };
            if selected.call.class != ExecutionEvidenceClass::Evidence
                || GroundingText::new(&selected.searchable_text())
                    .resolve(&citation.quote)
                    .is_err()
            {
                failures.push(GroundingFailure {
                    field_path: format!("{field_path}.tool_evidence"),
                    message: handle.clone(),
                    submitted: citation.quote.clone(),
                    reason: "tool_evidence_quote_not_found",
                });
            }
        }
        return;
    }

    if bypass {
        return;
    }

    for (handle, message_id, _, strength) in &assertions {
        if *strength != EvidenceStrength::Inferred
            && let Some(call) = evidence.strong_match_for_message(message_id, claim)
            && let Ok(supporting) = GroundingText::new(&call.searchable_text()).resolve(claim)
            && let Some(evidence_id) = evidence.evidence_id_for_quote(
                handle,
                message_id,
                &call.local_id,
                &supporting.raw_span,
            )
        {
            tool_citations.push(ToolEvidenceCitation {
                message: handle.clone(),
                evidence_id,
                quote: supporting.raw_span,
            });
            return;
        }
    }

    let has_qualified = assertions
        .iter()
        .any(|(_, message_id, _, _)| !evidence.qualified_for_message(message_id).is_empty());
    let (reason, detail) = if !has_qualified {
        (
            "agent_claim_without_tool_evidence",
            "No successful non-recall tool execution exists in the configured same-chat horizon."
                .to_string(),
        )
    } else {
        ("agent_claim_needs_tool_evidence",
            "Qualified executions exist; call search_tool_evidence for this Agent message and select returned evidence IDs.".to_string())
    };
    failures.push(GroundingFailure {
        field_path: field_path.to_string(),
        message: format!(
            "assertions={}; {detail}",
            assertions
                .iter()
                .map(|(handle, _, _, _)| handle.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ),
        submitted: claim.to_string(),
        reason,
    });
}

pub(super) fn batch_without_failed_contributions(
    batch: &Batch,
    failures: &[GroundingFailure],
) -> Batch {
    let mut cleaned = batch.clone();
    let playbook_paths = cleaned
        .playbooks
        .iter()
        .filter_map(|playbook| {
            normalize_path(&playbook.path).map(|path| (playbook.id.trim().to_string(), path))
        })
        .collect::<HashMap<_, _>>();
    for failure in failures
        .iter()
        .filter(|failure| failure.reason == "procedural_page_duplicates_playbook_candidate")
    {
        let Some(index) = failure
            .field_path
            .strip_prefix("memories[")
            .and_then(|tail| tail.split_once(']'))
            .and_then(|(index, _)| index.parse::<usize>().ok())
        else {
            continue;
        };
        let Some(memory) = cleaned.memories.get_mut(index) else {
            continue;
        };
        let Some(path) = memory
            .playbook
            .as_deref()
            .and_then(|id| playbook_paths.get(id.trim()))
        else {
            continue;
        };
        memory
            .entities
            .retain(|entity| normalize_path(entity).as_ref() != Some(path));
    }
    let mut clause_failures = HashMap::<usize, HashSet<&str>>::new();
    for failure in failures
        .iter()
        .filter(|failure| failure.reason == "tool_evidence_clause_mismatch")
    {
        let Some(index) = failure
            .field_path
            .strip_prefix("memories[")
            .and_then(|tail| tail.split_once(']'))
            .and_then(|(index, _)| index.parse::<usize>().ok())
        else {
            continue;
        };
        clause_failures
            .entry(index)
            .or_default()
            .insert(failure.message.as_str());
    }
    let mut empty_after_clause_cleanup = HashSet::new();
    for (index, rejected) in clause_failures {
        let Some(memory) = cleaned.memories.get_mut(index) else {
            continue;
        };
        let retained = claim_clauses(&memory.content)
            .into_iter()
            .filter(|clause| !rejected.contains(clause))
            .collect::<Vec<_>>();
        if retained.is_empty() {
            empty_after_clause_cleanup.insert(index);
        } else {
            memory.content = retained.join(" ");
        }
    }
    let mut memory_indices = failures
        .iter()
        .filter(|failure| {
            !matches!(
                failure.reason,
                "tool_evidence_clause_mismatch" | "procedural_page_duplicates_playbook_candidate"
            )
        })
        .filter_map(|failure| {
            failure
                .field_path
                .strip_prefix("memories[")?
                .split_once(']')?
                .0
                .parse::<usize>()
                .ok()
        })
        .collect::<Vec<_>>();
    memory_indices.extend(empty_after_clause_cleanup);
    memory_indices.sort_unstable();
    memory_indices.dedup();
    for index in memory_indices.into_iter().rev() {
        if index < cleaned.memories.len() {
            cleaned.memories.remove(index);
        }
    }
    let mut page_indices = failures
        .iter()
        .filter(|failure| {
            is_agent_evidence_failure(failure.reason)
                || failure.reason == "entity_path_duplicates_playbook_candidate"
        })
        .filter_map(|failure| {
            failure
                .field_path
                .strip_prefix("new_entities[")?
                .split_once(']')?
                .0
                .parse::<usize>()
                .ok()
        })
        .collect::<Vec<_>>();
    page_indices.sort_unstable();
    page_indices.dedup();
    for index in page_indices.into_iter().rev() {
        if index < cleaned.new_entities.len() {
            cleaned.new_entities.remove(index);
        }
    }
    remove_recalled_attributes(
        &mut cleaned.new_entities,
        "new_entities",
        failures,
        |entity| &mut entity.candidate_attributes,
    );
    remove_recalled_attributes(
        &mut cleaned.existing_entity_updates,
        "existing_entity_updates",
        failures,
        |entity| &mut entity.candidate_attributes,
    );
    cleaned
        .existing_entity_updates
        .retain(|entity| !entity.candidate_attributes.is_empty());
    cleaned
}

pub(super) fn remove_recalled_attributes<T>(
    entities: &mut [T],
    prefix: &str,
    failures: &[GroundingFailure],
    attributes: impl Fn(&mut T) -> &mut Vec<CandidateAttribute>,
) {
    for (page_index, entity) in entities.iter_mut().enumerate() {
        let marker = format!("{prefix}[{page_index}].candidate_attributes[");
        let mut indices = failures
            .iter()
            .filter(|failure| is_agent_evidence_failure(failure.reason))
            .filter_map(|failure| {
                failure
                    .field_path
                    .strip_prefix(&marker)?
                    .strip_suffix(']')?
                    .parse::<usize>()
                    .ok()
            })
            .collect::<Vec<_>>();
        indices.sort_unstable();
        indices.dedup();
        let held = attributes(entity);
        for index in indices.into_iter().rev() {
            if index < held.len() {
                held.remove(index);
            }
        }
    }
}

pub(super) fn is_agent_evidence_failure(reason: &str) -> bool {
    matches!(
        reason,
        "agent_claim_requires_evidence_search"
            | "agent_claim_recalled"
            | "agent_claim_without_tool_evidence"
            | "agent_claim_needs_tool_evidence"
            | "tool_evidence_without_agent_source"
            | "unknown_tool_evidence"
            | "tool_evidence_quote_not_found"
            | "tool_evidence_clause_mismatch"
            | "selected_evidence_missing_critical_values"
    )
}

pub(super) fn validate_citations(
    path: &str,
    citations: &[SourceCitation],
    sources: &[TranscriptEvidenceSource],
    failures: &mut Vec<GroundingFailure>,
) {
    if citations.is_empty() {
        failures.push(GroundingFailure {
            field_path: path.into(),
            message: String::new(),
            submitted: String::new(),
            reason: "missing_sources",
        });
        return;
    }
    let cited_handles: HashSet<&str> = citations
        .iter()
        .map(|citation| citation.message.as_str())
        .collect();
    for (index, citation) in citations.iter().enumerate() {
        if let Err(reason) = resolve_citation(citation, sources, &cited_handles) {
            let message = if reason == "quote_not_found"
                && sources
                    .iter()
                    .find(|source| source.handle == citation.message)
                    .is_some_and(|source| {
                        matches!(source.kind, TranscriptEvidenceKind::AgentMessage { .. })
                    }) {
                let candidates = sources
                    .iter()
                    .filter(|source| {
                        matches!(source.kind, TranscriptEvidenceKind::AgentMessage { .. })
                            && GroundingText::new(&source.text)
                                .resolve(&citation.quote)
                                .is_ok()
                    })
                    .map(|source| source.handle.as_str())
                    .collect::<Vec<_>>();
                if candidates.is_empty() {
                    citation.message.clone()
                } else {
                    format!(
                        "{}; exact quote appears in Agent messages: {}",
                        citation.message,
                        candidates.join(", ")
                    )
                }
            } else {
                citation.message.clone()
            };
            failures.push(GroundingFailure {
                field_path: format!("{path}[{index}].quote"),
                message,
                submitted: citation.quote.clone(),
                reason,
            });
        }
    }
}

pub(super) fn validate_attribute(
    path: &str,
    attribute: &CandidateAttribute,
    sources: &[TranscriptEvidenceSource],
    failures: &mut Vec<GroundingFailure>,
) {
    validate_citations(
        &format!("{path}.sources"),
        &attribute.sources,
        sources,
        failures,
    );
    if !value_in_declared_sources(&attribute.value, &attribute.sources, sources) {
        failures.push(GroundingFailure {
            field_path: format!("{path}.value"),
            message: String::new(),
            submitted: attribute.value.clone(),
            reason: "structured_value_not_found",
        });
    }
}

pub(super) fn value_in_declared_sources(
    value: &str,
    citations: &[SourceCitation],
    sources: &[TranscriptEvidenceSource],
) -> bool {
    citations
        .iter()
        .filter_map(|citation| {
            sources
                .iter()
                .find(|source| source.handle == citation.message)
        })
        .any(|source| {
            GroundingText::new(&source.text)
                .resolve_value(value)
                .is_ok()
        })
}

pub(super) fn candidate_attribute_evidence(
    candidate_attributes: &[CandidateAttribute],
    sources: &[TranscriptEvidenceSource],
    tool_evidence: &ToolEvidenceProjection,
) -> HashMap<String, Vec<MemoryEvidence>> {
    let mut evidence = HashMap::new();
    for attribute in candidate_attributes {
        let key = attribute.key.trim();
        if key.is_empty() {
            continue;
        }
        if let Some(resolved) = resolve_evidence_with_tools(
            &attribute.sources,
            &attribute.tool_evidence,
            sources,
            tool_evidence,
        ) {
            evidence.insert(key.to_string(), resolved);
        }
    }
    evidence
}

pub(super) fn resolve_evidence(
    citations: &[SourceCitation],
    sources: &[TranscriptEvidenceSource],
) -> Option<Vec<MemoryEvidence>> {
    if citations.is_empty() {
        return None;
    }
    let cited_handles: HashSet<&str> = citations
        .iter()
        .map(|citation| citation.message.as_str())
        .collect();
    citations
        .iter()
        .map(|citation| resolve_citation(citation, sources, &cited_handles).ok())
        .collect()
}

pub(super) fn resolve_evidence_with_tools(
    citations: &[SourceCitation],
    tool_citations: &[ToolEvidenceCitation],
    sources: &[TranscriptEvidenceSource],
    evidence: &ToolEvidenceProjection,
) -> Option<Vec<MemoryEvidence>> {
    let mut resolved = resolve_evidence(citations, sources)?;
    for citation in tool_citations {
        let assertion = citations
            .iter()
            .find(|source| source.message == citation.message)?;
        let transcript_source = sources
            .iter()
            .find(|source| source.handle == citation.message)?;
        let TranscriptEvidenceKind::AgentMessage { message_id, .. } = &transcript_source.kind
        else {
            return None;
        };
        let selected =
            evidence.resolve_evidence_id(&citation.message, message_id, &citation.evidence_id)?;
        let quote = GroundingText::new(&selected.searchable_text())
            .resolve(&citation.quote)
            .ok()?
            .raw_span;
        let support = selected.call.support_citation();
        let source = match support {
            ToolSupportCitation::WebSearch { .. } => EvidenceSource::WebSearch {
                message_id: selected.call.message_id.clone(),
                chat_id: selected.call.chat_id.clone(),
                tool_call_id: selected.call.tool_call_id.clone(),
                quote,
                query: selected.call.query.clone(),
                url: selected.call.url.clone(),
            },
            ToolSupportCitation::WebPage { .. } => EvidenceSource::WebPage {
                message_id: selected.call.message_id.clone(),
                chat_id: selected.call.chat_id.clone(),
                tool_call_id: selected.call.tool_call_id.clone(),
                quote,
                url: selected.call.url.clone(),
            },
            ToolSupportCitation::ToolResult { .. } => EvidenceSource::ToolResult {
                message_id: selected.call.message_id.clone(),
                chat_id: selected.call.chat_id.clone(),
                tool_call_id: selected.call.tool_call_id.clone(),
                quote,
            },
        };
        resolved.push(MemoryEvidence {
            strength: assertion.strength,
            source,
        });
    }
    Some(resolved)
}

pub(super) fn episode_anchor_is_grounded(
    anchor: &TemporalAnchor,
    sources: &[TranscriptEvidenceSource],
) -> bool {
    sources.iter().any(|source| {
        source.handle == anchor.message
            && anchor.quote.is_empty()
            && matches!(source.kind, TranscriptEvidenceKind::TaskLifecycle { .. })
    }) || citation_match(&anchor.message, &anchor.quote, sources).is_ok()
}

pub(super) fn citation_match(
    handle: &str,
    quote: &str,
    sources: &[TranscriptEvidenceSource],
) -> Result<String, &'static str> {
    let item = sources
        .iter()
        .find(|source| source.handle == handle)
        .ok_or("unknown_message")?;
    GroundingText::new(&item.text)
        .resolve(quote)
        .map(|matched| matched.raw_span)
        .map_err(|error| match error {
            TextGroundingError::QuoteNotFound => "quote_not_found",
        })
}

pub(super) fn resolve_citation(
    citation: &SourceCitation,
    sources: &[TranscriptEvidenceSource],
    cited_handles: &HashSet<&str>,
) -> Result<MemoryEvidence, &'static str> {
    let index = sources
        .iter()
        .position(|source| source.handle == citation.message)
        .ok_or("unknown_message")?;
    let item = &sources[index];
    let source = match &item.kind {
        TranscriptEvidenceKind::UserMessage {
            message_id,
            chat_id,
        } => {
            let quote = citation_match(&citation.message, &citation.quote, sources)?;
            if citation.confirmation {
                let previous = index
                    .checked_sub(1)
                    .and_then(|i| sources.get(i))
                    .ok_or("confirmation_without_cited_agent_claim")?;
                if !matches!(previous.kind, TranscriptEvidenceKind::AgentMessage { .. })
                    || !cited_handles.contains(previous.handle.as_str())
                {
                    return Err("confirmation_without_cited_agent_claim");
                }
                EvidenceSource::UserConfirmation {
                    message_id: message_id.clone(),
                    chat_id: chat_id.clone(),
                    quote,
                }
            } else {
                EvidenceSource::UserMessage {
                    message_id: message_id.clone(),
                    chat_id: chat_id.clone(),
                    quote,
                }
            }
        }
        TranscriptEvidenceKind::AgentMessage {
            message_id,
            agent_id,
            chat_id,
        } => {
            if citation.confirmation {
                return Err("confirmation_on_ineligible_source");
            }
            let quote = citation_match(&citation.message, &citation.quote, sources)?;
            EvidenceSource::AgentMessage {
                message_id: message_id.clone(),
                agent_id: agent_id.clone(),
                chat_id: chat_id.clone(),
                quote,
            }
        }
        TranscriptEvidenceKind::TaskLifecycle {
            message_id,
            chat_id,
            task_id,
        } => {
            if citation.confirmation {
                return Err("confirmation_on_ineligible_source");
            }
            EvidenceSource::TaskLifecycle {
                message_id: message_id.clone(),
                chat_id: chat_id.clone(),
                task_id: task_id.clone(),
            }
        }
        TranscriptEvidenceKind::ExternalNote { note } => {
            if citation.confirmation {
                return Err("confirmation_on_ineligible_source");
            }
            let quote = citation_match(&citation.message, &citation.quote, sources)?;
            EvidenceSource::ExternalNote {
                note: note.clone(),
                quote,
            }
        }
    };
    Ok(MemoryEvidence {
        strength: citation.strength,
        source,
    })
}
