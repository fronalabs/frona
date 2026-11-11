use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{Datelike, Timelike, Utc};
use frona_text::GroundingText;

use super::IngestInput;
use crate::evidence::{ToolEvidenceProjection, missing_critical_values};
use crate::ingest::correction::{GroundingFailure, claim_clauses, require_agent_evidence_search};
use crate::ingest::evidence::{
    episode_anchor_is_grounded, inherit_attribute_tool_support, resolve_citation, resolve_evidence,
    validate_agent_tool_grounding, validate_attribute, validate_citations,
};
use crate::ingest::submission::{
    Batch, ResearchDispositionResult, SourceCitation, ToolEvidenceCitation,
};
use crate::model::{AbsoluteTime, EpisodeStatus, EvidenceSource, MemoryEvidence, MemoryKind};
use crate::path::normalize_path;
use crate::{
    RecallProjection, ResearchCoverageStats, TemporalSource, TranscriptEvidenceKind,
    TranscriptEvidenceSource,
};

pub(super) fn validate_batch(
    batch: &Batch,
    sources: &[TranscriptEvidenceSource],
) -> Vec<GroundingFailure> {
    let mut failures = Vec::new();
    let playbook_paths = batch
        .playbooks
        .iter()
        .filter_map(|playbook| {
            normalize_path(&playbook.path).map(|path| (playbook.id.trim(), path))
        })
        .collect::<HashMap<_, _>>();
    let playbook_path_set = playbook_paths.values().cloned().collect::<HashSet<_>>();
    let entity_id_counts =
        batch
            .new_entities
            .iter()
            .fold(HashMap::<&str, usize>::new(), |mut counts, entity| {
                *counts.entry(entity.id.trim()).or_default() += 1;
                counts
            });
    let mut rejected_new_entities = HashSet::new();
    for (page_index, entity) in batch.new_entities.iter().enumerate() {
        let page_base = format!("new_entities[{page_index}]");
        let entity_id = entity.id.trim();
        if entity_id.is_empty() {
            failures.push(GroundingFailure {
                field_path: format!("{page_base}.id"),
                message: String::new(),
                submitted: entity.id.clone(),
                reason: "entity_id_required",
            });
        } else if entity_id_counts.get(entity_id).copied().unwrap_or_default() > 1 {
            failures.push(GroundingFailure {
                field_path: format!("{page_base}.id"),
                message: String::new(),
                submitted: entity.id.clone(),
                reason: "duplicate_entity_id",
            });
        }
        if normalize_path(&entity.path).is_some_and(|path| playbook_path_set.contains(&path)) {
            failures.push(GroundingFailure {
                field_path: format!("new_entities[{page_index}].path"),
                message: String::new(),
                submitted: entity.path.clone(),
                reason: "entity_path_duplicates_playbook_candidate",
            });
        }
        validate_citations(
            &format!("new_entities[{page_index}].sources"),
            &entity.sources,
            sources,
            &mut failures,
        );
        let cited_handles = entity
            .sources
            .iter()
            .map(|citation| citation.message.as_str())
            .collect::<HashSet<_>>();
        let has_valid_source = entity
            .sources
            .iter()
            .any(|citation| resolve_citation(citation, sources, &cited_handles).is_ok());
        if !has_valid_source && let Some(path) = normalize_path(&entity.path) {
            rejected_new_entities.insert(path);
        }
        for (attribute_index, attribute) in entity.candidate_attributes.iter().enumerate() {
            validate_attribute(
                &format!("new_entities[{page_index}].candidate_attributes[{attribute_index}]"),
                attribute,
                sources,
                &mut failures,
            );
        }
    }
    for (page_index, entity) in batch.existing_entity_updates.iter().enumerate() {
        for (attribute_index, attribute) in entity.candidate_attributes.iter().enumerate() {
            validate_attribute(
                &format!(
                    "existing_entity_updates[{page_index}].candidate_attributes[{attribute_index}]"
                ),
                attribute,
                sources,
                &mut failures,
            );
        }
    }
    let playbook_id_counts =
        batch
            .playbooks
            .iter()
            .fold(HashMap::<&str, usize>::new(), |mut counts, playbook| {
                *counts.entry(playbook.id.trim()).or_default() += 1;
                counts
            });
    let mut playbooks = HashSet::new();
    for (index, playbook) in batch.playbooks.iter().enumerate() {
        let base = format!("playbooks[{index}]");
        let id = playbook.id.trim();
        let mut valid = true;
        if id.is_empty() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.id"),
                message: String::new(),
                submitted: playbook.id.clone(),
                reason: "playbook_id_required",
            });
            valid = false;
        } else if playbook_id_counts.get(id).copied().unwrap_or_default() > 1 {
            failures.push(GroundingFailure {
                field_path: format!("{base}.id"),
                message: String::new(),
                submitted: playbook.id.clone(),
                reason: "duplicate_playbook_id",
            });
            valid = false;
        }
        if normalize_path(&playbook.path).is_none() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.path"),
                message: String::new(),
                submitted: playbook.path.clone(),
                reason: "playbook_path_invalid",
            });
            valid = false;
        }
        if playbook.name.trim().is_empty() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.name"),
                message: String::new(),
                submitted: playbook.name.clone(),
                reason: "playbook_name_required",
            });
            valid = false;
        }
        if playbook.description.trim().is_empty() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.description"),
                message: String::new(),
                submitted: playbook.description.clone(),
                reason: "playbook_description_required",
            });
            valid = false;
        }
        if valid {
            playbooks.insert(id);
        }
    }
    let available_playbooks = {
        let mut ids = playbooks.iter().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        format!("available candidates: {}", ids.join(", "))
    };
    for (memory_index, memory) in batch.memories.iter().enumerate() {
        let base = format!("memories[{memory_index}]");
        validate_citations(
            &format!("{base}.sources"),
            &memory.sources,
            sources,
            &mut failures,
        );
        let kind = MemoryKind::parse(&memory.kind);
        if kind.is_none() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.kind"),
                message: String::new(),
                submitted: memory.kind.clone(),
                reason: "invalid_memory_kind",
            });
        }
        if memory.content.trim().is_empty() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.content"),
                message: String::new(),
                submitted: memory.content.clone(),
                reason: "empty_memory_content",
            });
        }
        if !memory
            .entities
            .iter()
            .any(|path| normalize_path(path).is_some())
        {
            failures.push(GroundingFailure {
                field_path: format!("{base}.entities"),
                message: String::new(),
                submitted: memory.entities.join(", "),
                reason: "memory_has_no_usable_entity",
            });
        } else if memory
            .entities
            .iter()
            .filter_map(|path| normalize_path(path))
            .all(|path| rejected_new_entities.contains(&path))
        {
            failures.push(GroundingFailure {
                field_path: format!("{base}.entities"),
                message: String::new(),
                submitted: memory.entities.join(", "),
                reason: "memory_references_rejected_entities",
            });
        }
        if kind == Some(MemoryKind::Episodic) && memory.episode.is_none() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.episode"),
                message: String::new(),
                submitted: memory.kind.clone(),
                reason: "episodic_missing_episode",
            });
        } else if kind != Some(MemoryKind::Episodic) && memory.episode.is_some() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.episode"),
                message: String::new(),
                submitted: memory.kind.clone(),
                reason: "episode_for_non_episodic",
            });
        }
        if let Some(episode) = &memory.episode {
            if episode.duration.is_some() && episode.absolute.is_some() {
                failures.push(GroundingFailure {
                    field_path: format!("{base}.episode"),
                    message: episode.anchor.message.clone(),
                    submitted: episode.anchor.quote.clone(),
                    reason: "episode_has_duration_and_absolute",
                });
            }
            if !memory
                .sources
                .iter()
                .any(|source| source.message == episode.anchor.message)
            {
                failures.push(GroundingFailure {
                    field_path: format!("{base}.episode.anchor"),
                    message: episode.anchor.message.clone(),
                    submitted: episode.anchor.quote.clone(),
                    reason: "anchor_message_not_declared",
                });
            } else if !episode_anchor_is_grounded(&episode.anchor, sources) {
                failures.push(GroundingFailure {
                    field_path: format!("{base}.episode.anchor.quote"),
                    message: episode.anchor.message.clone(),
                    submitted: episode.anchor.quote.clone(),
                    reason: "anchor_quote_not_found",
                });
            }
        }
        let evidence = resolve_evidence(&memory.sources, sources);
        let procedural = kind == Some(MemoryKind::Procedural);
        if procedural
            && memory
                .playbook
                .as_deref()
                .and_then(|id| playbook_paths.get(id.trim()))
                .is_some_and(|playbook_path| {
                    memory
                        .entities
                        .iter()
                        .filter_map(|path| normalize_path(path))
                        .any(|entity_path| &entity_path == playbook_path)
                })
        {
            failures.push(GroundingFailure {
                field_path: format!("{base}.entities"),
                message: String::new(),
                submitted: memory.entities.join(", "),
                reason: "procedural_page_duplicates_playbook_candidate",
            });
        }
        if procedural
            && evidence
                .as_deref()
                .is_none_or(|items| distinct_assertion_message_count(items) != 1)
        {
            failures.push(GroundingFailure {
                field_path: format!("{base}.sources"),
                message: memory
                    .sources
                    .iter()
                    .map(|source| source.message.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                submitted: memory.content.clone(),
                reason: "procedural_requires_one_assertion_source",
            });
        }
        match (procedural, memory.playbook.as_deref().map(str::trim)) {
            (true, Some(id)) if playbooks.contains(id) => {}
            (true, None) => failures.push(GroundingFailure {
                field_path: format!("{base}.playbook"),
                message: String::new(),
                submitted: String::new(),
                reason: "procedural_playbook_missing",
            }),
            (true, Some(id)) => failures.push(GroundingFailure {
                field_path: format!("{base}.playbook"),
                message: available_playbooks.clone(),
                submitted: id.to_string(),
                reason: "unknown_playbook_candidate",
            }),
            (false, None) => {}
            (false, Some(id)) => failures.push(GroundingFailure {
                field_path: format!("{base}.playbook"),
                message: String::new(),
                submitted: id.to_string(),
                reason: "non_procedural_playbook_present",
            }),
        }
    }
    failures
}

pub(super) fn distinct_assertion_message_count(items: &[MemoryEvidence]) -> usize {
    items
        .iter()
        .filter_map(|item| match &item.source {
            EvidenceSource::UserMessage {
                chat_id,
                message_id,
                ..
            }
            | EvidenceSource::AgentMessage {
                chat_id,
                message_id,
                ..
            } => Some((chat_id.as_str(), message_id.as_str())),
            _ => None,
        })
        .collect::<HashSet<_>>()
        .len()
}

pub(super) fn validate_selected_evidence(
    field_path: &str,
    claim: &str,
    citations: &mut [SourceCitation],
    tool_citations: &mut [ToolEvidenceCitation],
    sources: &[TranscriptEvidenceSource],
    evidence: &ToolEvidenceProjection,
    failures: &mut Vec<GroundingFailure>,
) {
    let mut complete_evidence = Vec::new();
    let mut selected = Vec::new();
    let has_tool_evidence = !tool_citations.is_empty();
    for citation in citations {
        let Some(source) = sources
            .iter()
            .find(|source| source.handle == citation.message)
        else {
            continue;
        };
        let resolved = if citation.quote.is_empty()
            && matches!(source.kind, TranscriptEvidenceKind::TaskLifecycle { .. })
        {
            Some(source.text.clone())
        } else {
            GroundingText::new(&source.text)
                .resolve(&citation.quote)
                .ok()
                .map(|matched| matched.raw_span)
        };
        let Some(resolved) = resolved else { continue };
        citation.quote = resolved.clone();
        if !has_tool_evidence || !matches!(source.kind, TranscriptEvidenceKind::AgentMessage { .. })
        {
            complete_evidence.push(
                if matches!(source.kind, TranscriptEvidenceKind::TaskLifecycle { .. }) {
                    source.text.clone()
                } else {
                    resolved.clone()
                },
            );
        }
        selected.push(format!("- message {}: {:?}", citation.message, resolved));
    }
    for citation in tool_citations {
        let Some(source) = sources
            .iter()
            .find(|source| source.handle == citation.message)
        else {
            continue;
        };
        let TranscriptEvidenceKind::AgentMessage { message_id, .. } = &source.kind else {
            continue;
        };
        let Some(resolved) =
            evidence.resolve_evidence_id(&citation.message, message_id, &citation.evidence_id)
        else {
            continue;
        };
        let Ok(matched) = GroundingText::new(&resolved.searchable_text()).resolve(&citation.quote)
        else {
            continue;
        };
        citation.quote = matched.raw_span.clone();
        complete_evidence.push(resolved.call.critical_value_text());
        selected.push(format!(
            "- {} (message {}): {:?}",
            citation.evidence_id, citation.message, matched.raw_span,
        ));
    }
    if complete_evidence.is_empty() {
        return;
    }
    let complete_evidence = complete_evidence.join("\n");
    let clauses = claim_clauses(claim);
    if clauses.len() > 1 {
        for clause in clauses {
            let missing = missing_critical_values(clause, &complete_evidence);
            if missing.is_empty() {
                continue;
            }
            failures.push(GroundingFailure {
                field_path: format!("{field_path}.sources"),
                message: clause.to_string(),
                submitted: format!(
                    "unsupported clause: {clause:?}\nmissing critical values:\n{}\nselected evidence:\n{}\ncomparison ignores case, spaces, and punctuation",
                    missing.iter().map(|value| format!("- {value}")).collect::<Vec<_>>().join("\n"),
                    selected.join("\n"),
                ),
                reason: "tool_evidence_clause_mismatch",
            });
        }
    } else {
        let missing = missing_critical_values(claim, &complete_evidence);
        if missing.is_empty() {
            return;
        }
        failures.push(GroundingFailure {
            field_path: format!("{field_path}.sources"),
            message: selected.join("\n"),
            submitted: format!(
                "missing critical values:\n{}\nselected evidence:\n{}\ncomparison ignores case, spaces, and punctuation",
                missing.iter().map(|value| format!("- {value}")).collect::<Vec<_>>().join("\n"),
                selected.join("\n"),
            ),
            reason: "selected_evidence_missing_critical_values",
        });
    }
}

pub(super) fn validate_batch_with_recall(
    batch: &mut Batch,
    sources: &[TranscriptEvidenceSource],
    recall: &RecallProjection,
) -> Vec<GroundingFailure> {
    let mut failures = validate_batch(batch, sources);
    for (memory_index, memory) in batch.memories.iter_mut().enumerate() {
        let claim = memory.content.clone();
        validate_agent_tool_grounding(
            &format!("memories[{memory_index}]"),
            &claim,
            &mut memory.sources,
            &mut memory.tool_evidence,
            sources,
            &recall.evidence,
            &mut failures,
        );
        validate_selected_evidence(
            &format!("memories[{memory_index}]"),
            &claim,
            &mut memory.sources,
            &mut memory.tool_evidence,
            sources,
            &recall.evidence,
            &mut failures,
        );
    }
    inherit_attribute_tool_support(batch, &failures);
    for (page_index, entity) in batch.new_entities.iter_mut().enumerate() {
        for (attribute_index, attribute) in entity.candidate_attributes.iter_mut().enumerate() {
            let claim = format!("{}: {}", attribute.key, attribute.value);
            validate_agent_tool_grounding(
                &format!("new_entities[{page_index}].candidate_attributes[{attribute_index}]"),
                &claim,
                &mut attribute.sources,
                &mut attribute.tool_evidence,
                sources,
                &recall.evidence,
                &mut failures,
            );
            validate_selected_evidence(
                &format!("new_entities[{page_index}].candidate_attributes[{attribute_index}]"),
                &claim,
                &mut attribute.sources,
                &mut attribute.tool_evidence,
                sources,
                &recall.evidence,
                &mut failures,
            );
        }
    }
    for (page_index, entity) in batch.existing_entity_updates.iter_mut().enumerate() {
        for (attribute_index, attribute) in entity.candidate_attributes.iter_mut().enumerate() {
            let claim = format!("{}: {}", attribute.key, attribute.value);
            validate_agent_tool_grounding(
                &format!(
                    "existing_entity_updates[{page_index}].candidate_attributes[{attribute_index}]"
                ),
                &claim,
                &mut attribute.sources,
                &mut attribute.tool_evidence,
                sources,
                &recall.evidence,
                &mut failures,
            );
            validate_selected_evidence(
                &format!(
                    "existing_entity_updates[{page_index}].candidate_attributes[{attribute_index}]"
                ),
                &claim,
                &mut attribute.sources,
                &mut attribute.tool_evidence,
                sources,
                &recall.evidence,
                &mut failures,
            );
        }
    }
    failures
}

pub(super) fn validate_extract_submission(
    batch: &mut Batch,
    sources: &[TranscriptEvidenceSource],
    temporal_sources: &[TemporalSource],
    recall: &RecallProjection,
    searched_messages: &HashSet<String>,
    research_messages: &HashSet<String>,
    citation_repairs: &AtomicUsize,
) -> Vec<GroundingFailure> {
    citation_repairs.fetch_add(
        rebind_unique_agent_citations(batch, sources),
        Ordering::Relaxed,
    );
    let required = require_agent_evidence_search(batch, sources, recall, searched_messages);
    let mut failures = if required.is_empty() {
        validate_batch_with_recall(batch, sources, recall)
    } else {
        let mut failures = validate_batch(batch, sources);
        failures.extend(required);
        failures
    };
    failures.extend(validate_task_episode_times(batch, temporal_sources));
    failures.extend(validate_research_coverage(
        batch,
        sources,
        research_messages,
    ));
    failures
}

pub(super) fn validate_task_episode_times(
    batch: &Batch,
    sources: &[TemporalSource],
) -> Vec<GroundingFailure> {
    let mut failures = Vec::new();
    for (memory_index, memory) in batch.memories.iter().enumerate() {
        let Some(episode) = memory.episode.as_ref() else {
            continue;
        };
        let Some(source) = sources
            .iter()
            .find(|source| source.handle == episode.anchor.message)
        else {
            continue;
        };
        if source.task_event_at.is_none() && source.task_target_at.is_none() {
            continue;
        }
        let expected = match episode.status {
            EpisodeStatus::Planned => source.task_target_at.or(source.task_event_at),
            EpisodeStatus::Occurred | EpisodeStatus::Cancelled | EpisodeStatus::Unconfirmed => {
                source.task_event_at.or(source.task_target_at)
            }
        };
        let Some(expected) = expected else { continue };
        let base = format!("memories[{memory_index}].episode");
        if episode.duration.is_some() {
            failures.push(GroundingFailure {
                field_path: format!("{base}.duration"),
                message: episode.anchor.message.clone(),
                submitted: serde_json::to_string(&episode.duration).unwrap_or_default(),
                reason: "task_episode_uses_duration",
            });
        }
        match episode.absolute.as_ref() {
            None => failures.push(GroundingFailure {
                field_path: format!("{base}.absolute"),
                message: episode.anchor.message.clone(),
                submitted: format!("expected {}", expected.to_rfc3339()),
                reason: "task_episode_missing_absolute_time",
            }),
            Some(absolute) if !absolute_matches_utc(absolute, expected) => {
                failures.push(GroundingFailure {
                    field_path: format!("{base}.absolute"),
                    message: episode.anchor.message.clone(),
                    submitted: format!(
                        "received {}; expected {}",
                        serde_json::to_string(absolute).unwrap_or_default(),
                        expected.to_rfc3339(),
                    ),
                    reason: "task_episode_absolute_time_mismatch",
                });
            }
            Some(_) => {}
        }
    }
    failures
}

pub(super) fn absolute_matches_utc(
    absolute: &AbsoluteTime,
    expected: chrono::DateTime<Utc>,
) -> bool {
    absolute.year == Some(expected.year())
        && absolute.month == Some(expected.month())
        && absolute.day == Some(expected.day())
        && absolute.hour == Some(expected.hour())
        && absolute.minute == Some(expected.minute())
}

pub(super) fn research_message_handles(
    sources: &[TranscriptEvidenceSource],
    evidence: &ToolEvidenceProjection,
) -> HashSet<String> {
    sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| {
            let TranscriptEvidenceKind::AgentMessage { message_id, .. } = &source.kind else {
                return None;
            };
            if !evidence.has_direct_evidence(message_id)
                || sources.get(index + 1).is_some_and(|next| {
                    matches!(next.kind, TranscriptEvidenceKind::TaskLifecycle { .. })
                })
            {
                return None;
            }
            Some(source.handle.clone())
        })
        .collect()
}

fn citations_include_agent_message(
    citations: &[SourceCitation],
    sources: &[TranscriptEvidenceSource],
    message: &str,
) -> bool {
    citations.iter().any(|citation| {
        citation.message == message
            && sources.iter().any(|source| {
                source.handle == citation.message
                    && matches!(source.kind, TranscriptEvidenceKind::AgentMessage { .. })
            })
    })
}

fn contribution_cites_message(
    batch: &Batch,
    sources: &[TranscriptEvidenceSource],
    message: &str,
) -> bool {
    batch
        .memories
        .iter()
        .any(|memory| citations_include_agent_message(&memory.sources, sources, message))
        || batch
            .new_entities
            .iter()
            .flat_map(|entity| entity.candidate_attributes.iter())
            .any(|attribute| citations_include_agent_message(&attribute.sources, sources, message))
        || batch
            .existing_entity_updates
            .iter()
            .flat_map(|entity| entity.candidate_attributes.iter())
            .any(|attribute| citations_include_agent_message(&attribute.sources, sources, message))
}

pub(super) fn research_coverage_stats(
    batch: &Batch,
    sources: &[TranscriptEvidenceSource],
    research_messages: &HashSet<String>,
) -> ResearchCoverageStats {
    let mut stats = ResearchCoverageStats {
        messages: research_messages.len(),
        ..Default::default()
    };
    for message in research_messages {
        let contributed = contribution_cites_message(batch, sources, message);
        if contributed {
            stats.extracted += 1;
        } else if let Some(disposition) = batch
            .research_dispositions
            .iter()
            .find(|disposition| disposition.message == *message)
        {
            match disposition.result {
                ResearchDispositionResult::Extracted => {}
                ResearchDispositionResult::NoDurableClaim => stats.no_durable_claim += 1,
                ResearchDispositionResult::Duplicate => stats.duplicate += 1,
                ResearchDispositionResult::Unsupported => stats.unsupported += 1,
            }
        }
        if let Some(disposition) = batch
            .research_dispositions
            .iter()
            .find(|disposition| disposition.message == *message)
        {
            for claim in &disposition.claims {
                stats.claims += 1;
                match claim.result {
                    ResearchDispositionResult::Extracted => stats.claims_extracted += 1,
                    ResearchDispositionResult::NoDurableClaim => stats.claims_no_durable_claim += 1,
                    ResearchDispositionResult::Duplicate => stats.claims_duplicate += 1,
                    ResearchDispositionResult::Unsupported => stats.claims_unsupported += 1,
                }
            }
        }
    }
    stats
}

pub(super) fn rebind_unique_agent_citations(
    batch: &mut Batch,
    sources: &[TranscriptEvidenceSource],
) -> usize {
    fn repair(
        citations: &mut [SourceCitation],
        tool_citations: &mut [ToolEvidenceCitation],
        sources: &[TranscriptEvidenceSource],
    ) -> Vec<(String, String)> {
        let mut repairs = Vec::new();
        for citation in citations {
            if citation.quote.trim().is_empty() {
                continue;
            }
            let Some(declared) = sources
                .iter()
                .find(|source| source.handle == citation.message)
            else {
                continue;
            };
            if !matches!(declared.kind, TranscriptEvidenceKind::AgentMessage { .. })
                || GroundingText::new(&declared.text)
                    .resolve(&citation.quote)
                    .is_ok()
            {
                continue;
            }
            let matches = sources
                .iter()
                .filter(|source| {
                    matches!(source.kind, TranscriptEvidenceKind::AgentMessage { .. })
                        && GroundingText::new(&source.text)
                            .resolve(&citation.quote)
                            .is_ok()
                })
                .collect::<Vec<_>>();
            if let [actual] = matches.as_slice() {
                let old = std::mem::replace(&mut citation.message, actual.handle.clone());
                for tool in tool_citations.iter_mut().filter(|tool| tool.message == old) {
                    tool.message = actual.handle.clone();
                }
                repairs.push((old, actual.handle.clone()));
            }
        }
        repairs
    }

    let mut repaired = 0;
    for memory in &mut batch.memories {
        let changes = repair(&mut memory.sources, &mut memory.tool_evidence, sources);
        if let Some(episode) = &mut memory.episode {
            for (old, actual) in &changes {
                if episode.anchor.message == *old {
                    episode.anchor.message = actual.clone();
                }
            }
        }
        repaired += changes.len();
    }
    for entity in &mut batch.new_entities {
        repaired += repair(&mut entity.sources, &mut [], sources).len();
        for attribute in &mut entity.candidate_attributes {
            repaired += repair(
                &mut attribute.sources,
                &mut attribute.tool_evidence,
                sources,
            )
            .len();
        }
    }
    for entity in &mut batch.existing_entity_updates {
        for attribute in &mut entity.candidate_attributes {
            repaired += repair(
                &mut attribute.sources,
                &mut attribute.tool_evidence,
                sources,
            )
            .len();
        }
    }
    repaired
}

pub(super) fn validate_research_coverage(
    batch: &Batch,
    sources: &[TranscriptEvidenceSource],
    research_messages: &HashSet<String>,
) -> Vec<GroundingFailure> {
    let contribution_matches = |id: &str, message: &str| {
        batch.memories.iter().any(|memory| {
            memory.id == id && citations_include_agent_message(&memory.sources, sources, message)
        }) || batch
            .new_entities
            .iter()
            .flat_map(|entity| entity.candidate_attributes.iter())
            .chain(
                batch
                    .existing_entity_updates
                    .iter()
                    .flat_map(|entity| entity.candidate_attributes.iter()),
            )
            .any(|attribute| {
                attribute.id == id
                    && citations_include_agent_message(&attribute.sources, sources, message)
            })
    };
    let mut failures = Vec::new();
    for message in research_messages {
        let dispositions = batch
            .research_dispositions
            .iter()
            .filter(|disposition| disposition.message == *message)
            .collect::<Vec<_>>();
        match dispositions.as_slice() {
            [] => failures.push(GroundingFailure {
                field_path: "research_dispositions".into(),
                message: message.clone(),
                submitted: "no disposition or grounded contribution".into(),
                reason: "research_message_unaccounted",
            }),
            [disposition] => {
                if contribution_cites_message(batch, sources, message)
                    && disposition.result != ResearchDispositionResult::Extracted
                {
                    failures.push(GroundingFailure {
                        field_path: "research_dispositions".into(),
                        message: message.clone(),
                        submitted: disposition.reason.clone(),
                        reason: "research_disposition_conflicts_with_extraction",
                    });
                } else if !contribution_cites_message(batch, sources, message)
                    && disposition.result == ResearchDispositionResult::Extracted
                {
                    failures.push(GroundingFailure {
                        field_path: "research_dispositions".into(),
                        message: message.clone(),
                        submitted: disposition.reason.clone(),
                        reason: "research_extracted_without_contribution",
                    });
                }
                if disposition.reason.trim().is_empty() {
                    failures.push(GroundingFailure {
                        field_path: "research_dispositions".into(),
                        message: message.clone(),
                        submitted: String::new(),
                        reason: "research_disposition_requires_reason",
                    });
                }
                if disposition.claims.is_empty() {
                    failures.push(GroundingFailure {
                        field_path: "research_dispositions.claims".into(),
                        message: message.clone(),
                        submitted: "no claim-level coverage".into(),
                        reason: "research_claims_required",
                    });
                }
                for (index, claim) in disposition.claims.iter().enumerate() {
                    let path = format!("research_dispositions.claims[{index}]");
                    if claim.claim.trim().is_empty() {
                        failures.push(GroundingFailure {
                            field_path: path.clone(),
                            message: message.clone(),
                            submitted: String::new(),
                            reason: "research_claim_required",
                        });
                    }
                    if claim.result == ResearchDispositionResult::Extracted {
                        if claim.contribution_ids.is_empty()
                            || claim
                                .contribution_ids
                                .iter()
                                .any(|id| !contribution_matches(id, message))
                        {
                            failures.push(GroundingFailure {
                                field_path: path.clone(),
                                message: message.clone(),
                                submitted: claim.contribution_ids.join(", "),
                                reason: "research_claim_contribution_invalid",
                            });
                        }
                    } else if !claim.contribution_ids.is_empty() || claim.reason.trim().is_empty() {
                        failures.push(GroundingFailure {
                            field_path: path,
                            message: message.clone(),
                            submitted: claim.reason.clone(),
                            reason: "research_claim_disposition_invalid",
                        });
                    }
                }
            }
            _ => failures.push(GroundingFailure {
                field_path: "research_dispositions".into(),
                message: message.clone(),
                submitted: "multiple dispositions".into(),
                reason: "duplicate_research_disposition",
            }),
        }
    }
    failures
}

pub(super) fn validate_erroneous_memories(
    batch: &Batch,
    input: &IngestInput,
) -> Vec<GroundingFailure> {
    let mut failures = Vec::new();
    for (index, memory) in batch.memories.iter().enumerate() {
        let content = memory.content.trim().to_lowercase();
        if content.is_empty() {
            continue;
        }
        let mut rejected_paths = Vec::new();
        for path in memory
            .entities
            .iter()
            .filter_map(|path| normalize_path(path))
        {
            if input
                .erroneous_contents
                .get(&path)
                .is_some_and(|contents| contents.contains(&content))
            {
                rejected_paths.push(path);
            }
        }
        if !rejected_paths.is_empty() {
            failures.push(GroundingFailure {
                field_path: format!("memories[{index}].content"),
                message: format!("previously rejected on: {}", rejected_paths.join(", ")),
                submitted: memory.content.clone(),
                reason: "memory_was_previously_rejected",
            });
        }
    }
    failures
}
