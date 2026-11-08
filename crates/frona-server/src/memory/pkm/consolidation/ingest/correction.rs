use std::collections::{BTreeSet, HashMap, HashSet};

use crate::core::error::AppError;
use frona_harness::{ActionKind, ActionPosition, ExecutionReport};
use frona_text::GroundingText;

use crate::memory::pkm::consolidation::ingest::submission::{
    Batch, CandidateAttribute, NewMemory, SourceCitation,
};
use crate::memory::pkm::consolidation::{
    RecallProjection, ResearchCoverageStats, TranscriptEvidenceKind, TranscriptEvidenceSource,
};
use crate::memory::pkm::storage::normalize_path;

#[derive(Debug, Clone)]
pub(super) struct GroundingFailure {
    pub(super) field_path: String,
    pub(super) message: String,
    pub(super) submitted: String,
    pub(super) reason: &'static str,
}

#[derive(Default)]
pub(super) struct GroundingCorrectionState {
    pub(super) fingerprints: Vec<String>,
    pub(super) streak: usize,
    pub(super) retained: Option<Batch>,
    pub(super) failures: Vec<GroundingFailure>,
}

/// Grounding failures count as reported only after this exact rejection was
/// included in an admitted provider attempt. Sibling tool results, schema repair
/// and a summary covering old feedback do not establish literal delivery.
#[derive(Default)]
pub(super) struct GroundingFeedback {
    queued: Vec<(ActionPosition, Vec<String>)>,
}

impl GroundingFeedback {
    pub(super) fn queue(
        &mut self,
        fingerprints: Vec<String>,
        report: &ExecutionReport,
    ) -> Result<(), AppError> {
        // The refinement driver has resolved every sibling and leaves precisely
        // the selected submission pending while its inspector runs.
        let mut pending = report
            .actions
            .iter()
            .filter(|action| action.kind == ActionKind::Submission && action.outcome.is_none());
        let selected = pending
            .next()
            .filter(|_| pending.next().is_none())
            .ok_or_else(|| {
                AppError::Internal(
                    "extract cannot associate grounding feedback with its submission".into(),
                )
            })?;
        self.queued.push((selected.position, fingerprints));
        Ok(())
    }

    pub(super) fn has_unreported(&self, fingerprints: &[String], report: &ExecutionReport) -> bool {
        fingerprints.iter().any(|fingerprint| {
            !self.queued.iter().any(|(action, failures)| {
                failures.contains(fingerprint)
                    && report.feedback.iter().any(|feedback| {
                        feedback.action == Some(*action) && !feedback.issued_in.is_empty()
                    })
            })
        })
    }
}

#[derive(Debug, Default, Clone)]
pub(super) struct AgentEvidenceMetrics {
    pub(super) no_tool_drops: usize,
    pub(super) strong_matches: usize,
    pub(super) fallback_reviews: usize,
    pub(super) fallback_retains: usize,
    pub(super) invalid_submissions: usize,
    pub(super) lookup_calls: usize,
    pub(super) terminal_drops: usize,
    pub(super) research_coverage: ResearchCoverageStats,
}

impl GroundingFailure {
    pub(super) fn render_with_allowed(&self, allowed_override: Option<&[String]>) -> String {
        let allowed = allowed_override
            .map(|fields| fields.join(", "))
            .unwrap_or_else(|| self.allowed_fields().join(", "));
        let submitted = if matches!(
            self.reason,
            "tool_evidence_clause_mismatch" | "selected_evidence_missing_critical_values"
        ) {
            format!("details:\n{}", self.submitted)
        } else {
            format!("submitted: {:?}", self.submitted)
        };
        format!(
            "{}\nmessage: {}\n{}\nreason: {}\nrequired: {}\nallowed changes: {}",
            self.field_path,
            if self.message.is_empty() {
                "(none)"
            } else {
                &self.message
            },
            submitted,
            self.reason,
            grounding_reason_detail(self.reason),
            if allowed.is_empty() {
                "(none)"
            } else {
                &allowed
            },
        )
    }
    pub(super) fn fingerprint(&self) -> String {
        format!(
            "{} | {} | {} | {}",
            self.field_path,
            self.message,
            GroundingText::new(&self.submitted).comparison_stream(),
            self.reason
        )
    }

    pub(super) fn allowed_fields(&self) -> Vec<String> {
        let memory = self
            .field_path
            .split_once(".episode")
            .map(|(base, _)| base)
            .or_else(|| {
                self.field_path
                    .split_once(".playbook")
                    .map(|(base, _)| base)
            })
            .or_else(|| self.field_path.split_once(".sources").map(|(base, _)| base));
        match self.reason {
            "episode_for_non_episodic" | "episodic_missing_episode" => memory
                .map(|base| vec![format!("{base}.kind"), format!("{base}.episode")])
                .unwrap_or_default(),
            "episode_has_duration_and_absolute" => memory
                .map(|base| {
                    vec![
                        format!("{base}.episode.duration"),
                        format!("{base}.episode.absolute"),
                    ]
                })
                .unwrap_or_default(),
            "task_episode_missing_absolute_time"
            | "task_episode_absolute_time_mismatch"
            | "task_episode_uses_duration" => memory
                .map(|base| {
                    vec![
                        format!("{base}.episode.duration"),
                        format!("{base}.episode.absolute"),
                    ]
                })
                .unwrap_or_default(),
            "anchor_message_not_declared" => memory
                .map(|base| vec![format!("{base}.episode.anchor"), format!("{base}.sources")])
                .unwrap_or_default(),
            "anchor_quote_not_found" => memory
                .map(|base| vec![format!("{base}.episode.anchor")])
                .unwrap_or_default(),
            "procedural_requires_one_assertion_source" => memory
                .map(|base| vec![format!("{base}.kind"), format!("{base}.sources")])
                .unwrap_or_default(),
            "procedural_playbook_missing" | "unknown_playbook_candidate" => memory
                .map(|base| {
                    vec![
                        format!("{base}.kind"),
                        format!("{base}.playbook"),
                        "playbooks".into(),
                    ]
                })
                .unwrap_or_default(),
            "non_procedural_playbook_present" => memory
                .map(|base| vec![format!("{base}.kind"), format!("{base}.playbook")])
                .unwrap_or_default(),
            "invalid_memory_kind" => vec![self.field_path.clone()],
            "empty_memory_content" => vec![self.field_path.clone()],
            "memory_has_no_usable_entity" | "memory_references_rejected_entities" => {
                vec![self.field_path.clone()]
            }
            "procedural_page_duplicates_playbook_candidate" => memory
                .map(|base| {
                    vec![
                        format!("{base}.entities"),
                        format!("{base}.playbook"),
                        "playbooks".into(),
                    ]
                })
                .unwrap_or_default(),
            "memory_was_previously_rejected" => self
                .field_path
                .split_once(".content")
                .map(|(base, _)| vec![base.to_string()])
                .unwrap_or_default(),
            "playbook_id_required"
            | "duplicate_playbook_id"
            | "playbook_path_invalid"
            | "playbook_name_required"
            | "playbook_description_required"
            | "entity_path_duplicates_playbook_candidate" => {
                vec!["new_entities".into(), "playbooks".into()]
            }
            "entity_id_required" | "duplicate_entity_id" => {
                vec![self.field_path.clone()]
            }
            "entity_id_changed" => direct_new_entity_scope(&self.field_path)
                .into_iter()
                .collect(),
            "unknown_message"
            | "quote_not_found"
            | "confirmation_without_cited_agent_claim"
            | "confirmation_on_ineligible_source" => citation_scope(&self.field_path)
                .map(|scope| direct_new_entity_scope(&scope).unwrap_or(scope))
                .into_iter()
                .collect(),
            "agent_claim_requires_evidence_search"
            | "agent_claim_without_tool_evidence"
            | "agent_claim_needs_tool_evidence"
            | "tool_evidence_without_agent_source"
            | "unknown_tool_evidence"
            | "tool_evidence_quote_not_found"
            | "tool_evidence_clause_mismatch" => {
                let base = self
                    .field_path
                    .split_once(".sources")
                    .map(|(base, _)| base)
                    .or_else(|| {
                        self.field_path
                            .split_once(".tool_evidence")
                            .map(|(base, _)| base)
                    })
                    .unwrap_or(self.field_path.as_str());
                vec![
                    base.to_string(),
                    format!("{base}.sources"),
                    format!("{base}.tool_evidence"),
                ]
            }
            "missing_sources" | "structured_value_not_found" => {
                if let Some(entity) = citation_scope(&self.field_path)
                    .and_then(|scope| direct_new_entity_scope(&scope))
                {
                    vec![entity]
                } else {
                    citation_scope(&self.field_path)
                        .into_iter()
                        .chain(std::iter::once(self.field_path.clone()))
                        .collect()
                }
            }
            "selected_evidence_missing_critical_values" => {
                let base = self
                    .field_path
                    .split_once(".sources")
                    .map(|(base, _)| base)
                    .or_else(|| {
                        self.field_path
                            .split_once(".tool_evidence")
                            .map(|(base, _)| base)
                    })
                    .unwrap_or(self.field_path.as_str());
                vec![base.to_string()]
            }
            "research_message_unaccounted"
            | "research_extracted_without_contribution"
            | "research_disposition_requires_reason"
            | "duplicate_research_disposition"
            | "research_disposition_conflicts_with_extraction"
            | "research_claims_required"
            | "research_claim_required"
            | "research_claim_contribution_invalid"
            | "research_claim_disposition_invalid" => vec![
                "new_entities".into(),
                "existing_entity_updates".into(),
                "playbooks".into(),
                "memories".into(),
                "research_dispositions".into(),
            ],
            _ => Vec::new(),
        }
    }
}

pub(super) fn grounding_reason_detail(reason: &str) -> &'static str {
    match reason {
        "episode_for_non_episodic" => {
            "An episode is allowed only when kind is Episodic; preserve the dated occurrence by changing kind or episode together."
        }
        "episodic_missing_episode" => {
            "An Episodic memory requires an episode; add its grounded temporal data or change kind if the claim is actually durable."
        }
        "episode_has_duration_and_absolute" => {
            "An episode may normalize its anchor as duration or absolute time, never both."
        }
        "task_episode_missing_absolute_time" => {
            "A task lifecycle episode must copy the applicable task timestamp into episode.absolute. Use target_at for a planned episode. Use event_at for an occurred, failed, cancelled, or unconfirmed episode. Copy all UTC components through the minute and leave duration empty."
        }
        "task_episode_absolute_time_mismatch" => {
            "The task lifecycle episode time must match its applicable task timestamp. Use target_at for a planned episode. Use event_at for an occurred, failed, cancelled, or unconfirmed episode. Copy all UTC components through the minute and leave duration empty."
        }
        "task_episode_uses_duration" => {
            "A task lifecycle source has an authoritative timestamp. Remove episode.duration and copy the applicable UTC timestamp into episode.absolute."
        }
        "confirmation_without_cited_agent_claim" => {
            "A User confirmation must immediately follow, and cite alongside, the Agent claim it confirms."
        }
        "confirmation_on_ineligible_source" => "Only a User citation may set confirmation=true.",
        "procedural_requires_one_assertion_source" => {
            "A Procedural memory must cite exactly one User or Agent assertion message."
        }
        "procedural_playbook_missing" => {
            "A Procedural memory must reference exactly one submitted playbook candidate."
        }
        "non_procedural_playbook_present" => {
            "Only Procedural memories may reference a playbook candidate."
        }
        "unknown_playbook_candidate" => {
            "The playbook ID must name a candidate in this same submission."
        }
        "invalid_memory_kind" => {
            "Use one supported memory kind: Identity, Preference, Fact, Reference, Episodic, or Procedural."
        }
        "empty_memory_content" => "A memory must contain a durable claim.",
        "memory_has_no_usable_entity" => {
            "A memory must link to at least one entity path with a usable alphanumeric segment."
        }
        "procedural_page_duplicates_playbook_candidate" => {
            "A Procedural memory links entities through entities and links its procedure only through playbook. Remove the candidate Playbook path from entities."
        }
        "memory_was_previously_rejected" => {
            "This exact memory was previously marked erroneous on a linked entity. Drop it. Keep a revised memory only when the transcript states materially different knowledge."
        }
        "playbook_id_required" => "A playbook candidate requires a non-empty request-local ID.",
        "duplicate_playbook_id" => "Each playbook candidate requires a unique request-local ID.",
        "playbook_path_invalid" => {
            "A playbook candidate requires an entity path with a usable alphanumeric segment."
        }
        "playbook_name_required" => "A playbook candidate requires a non-empty name.",
        "playbook_description_required" => "A playbook candidate requires a non-empty description.",
        "entity_id_required" => "A new entity requires a non-empty request-local ID.",
        "duplicate_entity_id" => "Each new entity requires a unique request-local ID.",
        "entity_id_changed" => {
            "Keep the original request-local entity ID. You may change the entity path, name, description, aliases, sources, and candidate attributes."
        }
        "entity_path_duplicates_playbook_candidate" => {
            "One path cannot be both a new entity entity and a provisional Playbook. Remove the Playbook from new_entities and keep it only in playbooks."
        }
        "memory_references_rejected_entities" => {
            "Every linked new entity lacks valid source evidence. Fix a linked entity, link an existing grounded entity, or drop this memory."
        }
        "selected_evidence_missing_critical_values" => {
            "The claim and its complete selected evidence must use the same critical numbers, dates, URLs, and identifiers. Rewrite the claim literally, repair its evidence, or drop it."
        }
        "anchor_message_not_declared" => {
            "The episode anchor message must also occur in the memory source citations."
        }
        "anchor_quote_not_found" => {
            "The episode anchor quote must be an exact span in its declared message."
        }
        "missing_sources" => "At least one grounded source citation is required.",
        "structured_value_not_found" => {
            "The structured value must occur in one of its declared source messages."
        }
        "quote_not_found" => "The quote must be an exact span in its declared message.",
        "unknown_message" => {
            "The citation must use a source handle supplied in this extraction request."
        }
        "agent_claim_recalled" => {
            "This Agent claim is supported by prior knowledge retrieval attached to the same message. Omit it unless a later User message confirms, corrects, changes, or extends it, or retain only a genuinely new durable portion."
        }
        "agent_claim_without_tool_evidence" => {
            "This Agent-sourced claim has no successful non-recall tool execution in its evidence horizon. Drop the claim; recall and model knowledge cannot support it."
        }
        "agent_claim_requires_evidence_search" => {
            "Call search_tool_evidence for this Agent message before submitting the contribution, then select returned evidence IDs or drop it."
        }
        "agent_claim_needs_tool_evidence" => {
            "Qualified tool executions exist, but code found no near-verbatim support. Select returned evidence IDs with exact tool quotes, reformulate the claim to what the evidence supports, or drop it."
        }
        "tool_evidence_without_agent_source" => {
            "Every tool-evidence message must also occur as an Agent assertion source in the same contribution. Add that exact Agent citation or remove the unrelated tool evidence."
        }
        "unknown_tool_evidence" => {
            "The evidence ID was not returned for this Agent message in this extraction conversation."
        }
        "tool_evidence_quote_not_found" => {
            "The tool-evidence quote must be an exact span of the selected evidence chunk's sanitized request or response."
        }
        "tool_evidence_clause_mismatch" => {
            "The complete selected evidence does not support this clause. Split the memory and preserve every supported clause."
        }
        "research_message_unaccounted" => {
            "This Agent message has successful non-recall research executions. Add grounded contributions from it or give one explicit no_durable_claim, duplicate, or unsupported disposition with a reason."
        }
        "research_extracted_without_contribution" => {
            "An extracted disposition requires at least one submitted memory or candidate attribute sourced from this Agent message."
        }
        "research_disposition_requires_reason" => {
            "A non-extracted research disposition requires a short reason."
        }
        "duplicate_research_disposition" => {
            "Return exactly one disposition for this research message."
        }
        "research_disposition_conflicts_with_extraction" => {
            "A message that supplies a contribution cannot also be marked no_durable_claim, duplicate, or unsupported."
        }
        "research_claims_required" => {
            "List each material durable claim from this research message and record whether it was extracted, unsupported, duplicate, or not durable."
        }
        "research_claim_required" => {
            "A claim-level coverage item requires the concrete claim that was evaluated."
        }
        "research_claim_contribution_invalid" => {
            "An extracted claim must name one or more submitted memory or candidate-attribute IDs that cite this research message."
        }
        "research_claim_disposition_invalid" => {
            "A claim that was not extracted must have no contribution IDs and must give a concrete reason."
        }
        _ => "Revise only the rejected field while preserving every accepted field.",
    }
}

pub(super) fn claim_clauses(claim: &str) -> Vec<&str> {
    claim
        .split_inclusive(['.', ';', '\n'])
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .collect()
}

pub(super) fn citation_scope(path: &str) -> Option<String> {
    path.find(".sources[")
        .map(|end| format!("{}.sources", &path[..end]))
        .or_else(|| path.ends_with(".sources").then(|| path.to_string()))
}

pub(super) fn direct_new_entity_scope(path: &str) -> Option<String> {
    let tail = path.strip_prefix("new_entities[")?;
    let (index, suffix) = tail.split_once(']')?;
    (!suffix.contains(".candidate_attributes[")).then(|| format!("new_entities[{index}]"))
}

pub(super) fn canonical_failure_state(failures: &[GroundingFailure]) -> Vec<String> {
    let mut values = failures
        .iter()
        .map(GroundingFailure::fingerprint)
        .collect::<Vec<_>>();
    values.sort();
    values
}

pub(super) fn correction_memory_ids(
    batch: &Batch,
    failures: &[GroundingFailure],
) -> (Vec<String>, Vec<String>) {
    let repairs = failures
        .iter()
        .filter_map(|failure| {
            failure
                .field_path
                .strip_prefix("memories[")?
                .split_once(']')?
                .0
                .parse::<usize>()
                .ok()
        })
        .collect::<BTreeSet<_>>();
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for (index, memory) in batch.memories.iter().enumerate() {
        let id = if memory.id.trim().is_empty() {
            format!("memories[{index}]")
        } else {
            memory.id.clone()
        };
        if repairs.contains(&index) {
            rejected.push(id);
        } else {
            accepted.push(id);
        }
    }
    (accepted, rejected)
}

pub(super) fn render_memory_evidence(batch: &Batch, ids: &[String]) -> String {
    if ids.is_empty() {
        return "(none)".to_string();
    }
    ids.iter()
        .map(|id| {
            let memory = batch
                .memories
                .iter()
                .enumerate()
                .find(|(index, memory)| {
                    if memory.id.trim().is_empty() {
                        id == &format!("memories[{index}]")
                    } else {
                        id == &memory.id
                    }
                })
                .map(|(_, memory)| memory);
            let Some(memory) = memory else {
                return format!("- `{id}`");
            };
            let mut evidence = memory
                .sources
                .iter()
                .map(|citation| citation.message.clone())
                .chain(
                    memory
                        .tool_evidence
                        .iter()
                        .map(|citation| citation.evidence_id.clone()),
                )
                .collect::<Vec<_>>();
            evidence.sort();
            evidence.dedup();
            format!(
                "- `{id}` -> {}",
                if evidence.is_empty() {
                    "(no evidence)".to_string()
                } else {
                    evidence.join(", ")
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn count_tool_supports(batch: &Batch) -> usize {
    batch
        .memories
        .iter()
        .map(|memory| memory.tool_evidence.len())
        .sum::<usize>()
        + batch
            .new_entities
            .iter()
            .flat_map(|entity| entity.candidate_attributes.iter())
            .map(|attribute| attribute.tool_evidence.len())
            .sum::<usize>()
        + batch
            .existing_entity_updates
            .iter()
            .flat_map(|entity| entity.candidate_attributes.iter())
            .map(|attribute| attribute.tool_evidence.len())
            .sum::<usize>()
}

pub(super) fn contribution_count(batch: &Batch) -> usize {
    batch.memories.len()
        + batch.new_entities.len()
        + batch.existing_entity_updates.len()
        + batch.playbooks.len()
        + batch
            .new_entities
            .iter()
            .map(|entity| entity.candidate_attributes.len())
            .sum::<usize>()
        + batch
            .existing_entity_updates
            .iter()
            .map(|entity| entity.candidate_attributes.len())
            .sum::<usize>()
}

pub(super) fn agent_contribution_count(
    batch: &Batch,
    sources: &[TranscriptEvidenceSource],
) -> usize {
    let agent_sourced = |citations: &[SourceCitation]| {
        citations.iter().any(|citation| {
            sources.iter().any(|source| {
                source.handle == citation.message
                    && matches!(source.kind, TranscriptEvidenceKind::AgentMessage { .. })
            })
        })
    };
    batch
        .memories
        .iter()
        .filter(|memory| agent_sourced(&memory.sources))
        .count()
        + batch
            .new_entities
            .iter()
            .flat_map(|entity| entity.candidate_attributes.iter())
            .filter(|attribute| agent_sourced(&attribute.sources))
            .count()
        + batch
            .existing_entity_updates
            .iter()
            .flat_map(|entity| entity.candidate_attributes.iter())
            .filter(|attribute| agent_sourced(&attribute.sources))
            .count()
}

pub(super) fn require_agent_evidence_search(
    batch: &Batch,
    sources: &[TranscriptEvidenceSource],
    recall: &RecallProjection,
    searched: &HashSet<String>,
) -> Vec<GroundingFailure> {
    let missing = |citations: &[SourceCitation]| {
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
        if bypass {
            return Vec::new();
        }
        let mut missing = citations
            .iter()
            .filter_map(|citation| {
                let source = sources
                    .iter()
                    .find(|source| source.handle == citation.message)?;
                let TranscriptEvidenceKind::AgentMessage { message_id, .. } = &source.kind else {
                    return None;
                };
                if !recall.result_calls_for_message(message_id).is_empty() {
                    return None;
                }
                (!searched.contains(&citation.message)).then(|| citation.message.clone())
            })
            .collect::<Vec<_>>();
        missing.sort();
        missing.dedup();
        missing
    };
    let mut failures = Vec::new();
    for (index, memory) in batch.memories.iter().enumerate() {
        for message in if memory.tool_evidence.is_empty() {
            missing(&memory.sources)
        } else {
            Vec::new()
        } {
            failures.push(GroundingFailure {
                field_path: format!("memories[{index}].sources"),
                message,
                submitted: memory.content.clone(),
                reason: "agent_claim_requires_evidence_search",
            });
        }
    }
    for (page_index, entity) in batch.new_entities.iter().enumerate() {
        for (attribute_index, attribute) in entity.candidate_attributes.iter().enumerate() {
            for message in if attribute.tool_evidence.is_empty() {
                missing(&attribute.sources)
            } else {
                Vec::new()
            } {
                failures.push(GroundingFailure {
                    field_path: format!(
                        "new_entities[{page_index}].candidate_attributes[{attribute_index}].sources"
                    ),
                    message,
                    submitted: format!("{}: {}", attribute.key, attribute.value),
                    reason: "agent_claim_requires_evidence_search",
                });
            }
        }
    }
    for (page_index, entity) in batch.existing_entity_updates.iter().enumerate() {
        for (attribute_index, attribute) in entity.candidate_attributes.iter().enumerate() {
            for message in if attribute.tool_evidence.is_empty() {
                missing(&attribute.sources)
            } else {
                Vec::new()
            } {
                failures.push(GroundingFailure {
                    field_path: format!("existing_entity_updates[{page_index}].candidate_attributes[{attribute_index}].sources"),
                    message, submitted: format!("{}: {}", attribute.key, attribute.value),
                    reason: "agent_claim_requires_evidence_search",
                });
            }
        }
    }
    failures
}

pub(super) fn apply_allowed_revision(
    previous: &Batch,
    revised: &Batch,
    failures: &[GroundingFailure],
) -> Batch {
    let coverage_repair = failures.iter().any(|failure| {
        matches!(
            failure.reason,
            "research_message_unaccounted"
                | "research_extracted_without_contribution"
                | "research_disposition_requires_reason"
                | "duplicate_research_disposition"
                | "research_disposition_conflicts_with_extraction"
                | "research_claims_required"
                | "research_claim_required"
                | "research_claim_contribution_invalid"
                | "research_claim_disposition_invalid"
        )
    });
    let mut allowed = failures
        .iter()
        .flat_map(GroundingFailure::allowed_fields)
        .collect::<Vec<_>>();
    if coverage_repair {
        allowed.retain(|path| {
            !matches!(
                path.as_str(),
                "new_entities"
                    | "existing_entity_updates"
                    | "playbooks"
                    | "memories"
                    | "research_dispositions"
            )
        });
    }
    allowed.sort_by_key(String::len);
    allowed.dedup();
    let mut roots = Vec::<String>::new();
    for path in allowed {
        if !roots.iter().any(|root| {
            path == *root
                || path
                    .strip_prefix(root)
                    .is_some_and(|tail| tail.starts_with('.') || tail.starts_with('['))
        }) {
            roots.push(path);
        }
    }
    let dropped_memory_indices = failures
        .iter()
        .filter_map(|failure| {
            failure
                .field_path
                .strip_prefix("memories[")?
                .split_once(']')?
                .0
                .parse::<usize>()
                .ok()
        })
        .collect::<BTreeSet<_>>();
    let mut base = previous.clone();
    if coverage_repair {
        merge_research_coverage_revision(&mut base, revised);
    }
    let direct_failed_pages = direct_failed_entity_indices(failures);
    let mut entity_path_changes = Vec::new();
    for index in direct_failed_pages.iter().copied().rev() {
        let Some(previous_page) = previous.new_entities.get(index) else {
            continue;
        };
        let invalid_id = failures.iter().any(|failure| {
            failure
                .field_path
                .starts_with(&format!("new_entities[{index}].id"))
                && matches!(failure.reason, "entity_id_required" | "duplicate_entity_id")
        });
        let revised_page = if invalid_id {
            revised
                .new_entities
                .iter()
                .find(|candidate| candidate.path == previous_page.path)
                .or_else(|| revised.new_entities.get(index))
        } else {
            revised.new_entities.iter().find(|candidate| {
                !previous_page.id.trim().is_empty() && candidate.id == previous_page.id
            })
        };
        if let Some(revised_page) = revised_page {
            if previous_page.path != revised_page.path {
                entity_path_changes.push((previous_page.path.clone(), revised_page.path.clone()));
            }
            if let Some(target) = base.new_entities.get_mut(index) {
                *target = revised_page.clone();
            }
        } else if !has_unexpected_entity_ids(previous, revised) && index < base.new_entities.len() {
            base.new_entities.remove(index);
        }
    }
    for (before, after) in entity_path_changes {
        rewrite_memory_entity_references(&mut base.memories, &before, &after);
    }
    let stable_memory_revision = !dropped_memory_indices.is_empty()
        && previous
            .memories
            .iter()
            .all(|memory| !memory.id.trim().is_empty())
        && revised
            .memories
            .iter()
            .all(|memory| !memory.id.trim().is_empty());
    if stable_memory_revision {
        let previous_ids = previous
            .memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<HashSet<_>>();
        let failed = previous
            .memories
            .iter()
            .enumerate()
            .filter(|(index, _)| dropped_memory_indices.contains(index))
            .map(|(_, memory)| memory)
            .collect::<Vec<_>>();
        base.memories = previous
            .memories
            .iter()
            .enumerate()
            .filter_map(|(index, memory)| {
                if dropped_memory_indices.contains(&index)
                    && !revised
                        .memories
                        .iter()
                        .any(|candidate| candidate.id == memory.id)
                {
                    None
                } else {
                    Some(memory.clone())
                }
            })
            .collect();
        for candidate in revised
            .memories
            .iter()
            .filter(|memory| !previous_ids.contains(memory.id.as_str()))
        {
            if failed
                .iter()
                .any(|memory| plausible_memory_revision(memory, candidate))
                && !base.memories.iter().any(|memory| memory.id == candidate.id)
            {
                base.memories.push(candidate.clone());
            }
        }
    }
    let memory_array_reconciled = !stable_memory_revision
        && !dropped_memory_indices.is_empty()
        && previous.memories.len() != revised.memories.len();
    if memory_array_reconciled {
        let mut remaining = revised.memories.clone();
        for (index, memory) in previous.memories.iter().enumerate() {
            if dropped_memory_indices.contains(&index) {
                continue;
            }
            if let Some(position) = remaining.iter().position(|candidate| {
                candidate.content == memory.content
                    && candidate.kind == memory.kind
                    && candidate.entities == memory.entities
                    && candidate.playbook == memory.playbook
            }) {
                remaining.remove(position);
            }
        }
        let failed = previous
            .memories
            .iter()
            .enumerate()
            .filter(|(index, _)| dropped_memory_indices.contains(index))
            .map(|(_, memory)| memory)
            .collect::<Vec<_>>();
        base.memories = previous
            .memories
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped_memory_indices.contains(index))
            .map(|(_, memory)| memory.clone())
            .collect();
        for candidate in remaining {
            if failed
                .iter()
                .any(|memory| plausible_memory_revision(memory, &candidate))
                && !base.memories.iter().any(|memory| {
                    memory.kind == candidate.kind
                        && memory.content == candidate.content
                        && memory.entities == candidate.entities
                        && memory.playbook == candidate.playbook
                })
            {
                base.memories.push(candidate);
            }
        }
    }
    reconcile_failed_attributes(
        &mut base.new_entities,
        &revised.new_entities,
        "new_entities",
        failures,
        |entity| &entity.path,
        |entity| &mut entity.candidate_attributes,
        |entity| &entity.candidate_attributes,
    );
    reconcile_failed_attributes(
        &mut base.existing_entity_updates,
        &revised.existing_entity_updates,
        "existing_entity_updates",
        failures,
        |entity| &entity.path,
        |entity| &mut entity.candidate_attributes,
        |entity| &entity.candidate_attributes,
    );
    let mut merged = serde_json::to_value(&base).expect("extract batch serializes");
    let revised_value = serde_json::to_value(revised).expect("extract batch serializes");
    for path in roots {
        if coverage_repair
            && matches!(
                path.as_str(),
                "new_entities"
                    | "existing_entity_updates"
                    | "playbooks"
                    | "memories"
                    | "research_dispositions"
            )
        {
            continue;
        }
        if memory_array_reconciled && path.starts_with("memories[") {
            continue;
        }
        if direct_new_entity_scope(&path)
            .and_then(|scope| {
                scope
                    .strip_prefix("new_entities[")?
                    .strip_suffix(']')?
                    .parse::<usize>()
                    .ok()
            })
            .is_some_and(|index| direct_failed_pages.contains(&index))
        {
            continue;
        }
        if path.contains(".candidate_attributes[") {
            continue;
        }
        let target_path =
            current_memory_path(&path, previous, &base).unwrap_or_else(|| path.clone());
        let target_pointer = field_path_pointer(&target_path);
        let source_path =
            revised_memory_path(&path, previous, Some(revised)).unwrap_or_else(|| path.clone());
        let source_pointer = field_path_pointer(&source_path);
        let Some(value) = revised_value.pointer(&source_pointer).cloned() else {
            continue;
        };
        if let Some(target) = merged.pointer_mut(&target_pointer) {
            *target = value;
        }
    }
    serde_json::from_value(merged).expect("allowed extraction revision remains a batch")
}

pub(super) fn direct_failed_entity_indices(failures: &[GroundingFailure]) -> BTreeSet<usize> {
    failures
        .iter()
        .filter_map(|failure| {
            let tail = failure.field_path.strip_prefix("new_entities[")?;
            let (index, suffix) = tail.split_once(']')?;
            if suffix.contains(".candidate_attributes[") {
                return None;
            }
            index.parse::<usize>().ok()
        })
        .collect()
}

pub(super) fn validate_entity_revision_identity(
    previous: &Batch,
    revised: &Batch,
    failures: &[GroundingFailure],
) -> Vec<GroundingFailure> {
    if failures.iter().any(|failure| {
        matches!(
            failure.reason,
            "research_message_unaccounted"
                | "research_extracted_without_contribution"
                | "research_disposition_requires_reason"
                | "duplicate_research_disposition"
                | "research_disposition_conflicts_with_extraction"
                | "research_claims_required"
                | "research_claim_required"
                | "research_claim_contribution_invalid"
                | "research_claim_disposition_invalid"
        )
    }) {
        return Vec::new();
    }
    let id_counts =
        previous
            .new_entities
            .iter()
            .fold(HashMap::<&str, usize>::new(), |mut counts, entity| {
                *counts.entry(entity.id.trim()).or_default() += 1;
                counts
            });
    let expected = direct_failed_entity_indices(failures)
        .into_iter()
        .filter_map(|index| Some((index, previous.new_entities.get(index)?)))
        .filter(|(_, entity)| {
            !entity.id.trim().is_empty()
                && id_counts.get(entity.id.trim()).copied() == Some(1)
                && !revised
                    .new_entities
                    .iter()
                    .any(|candidate| candidate.id == entity.id)
        })
        .map(|(index, entity)| (index, entity.id.as_str()))
        .collect::<Vec<_>>();
    if expected.is_empty() {
        return Vec::new();
    }
    let known = previous
        .new_entities
        .iter()
        .map(|entity| entity.id.as_str())
        .collect::<HashSet<_>>();
    revised
        .new_entities
        .iter()
        .filter(|entity| !known.contains(entity.id.as_str()))
        .enumerate()
        .map(|(position, entity)| GroundingFailure {
            field_path: format!(
                "new_entities[{}].id",
                expected[position.min(expected.len() - 1)].0
            ),
            message: format!(
                "expected retained entity ID: {}",
                expected
                    .iter()
                    .map(|(_, id)| *id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            submitted: entity.id.clone(),
            reason: "entity_id_changed",
        })
        .collect()
}

pub(super) fn has_unexpected_entity_ids(previous: &Batch, revised: &Batch) -> bool {
    let known = previous
        .new_entities
        .iter()
        .map(|entity| entity.id.as_str())
        .collect::<HashSet<_>>();
    revised
        .new_entities
        .iter()
        .any(|entity| !known.contains(entity.id.as_str()))
}

pub(super) fn rewrite_memory_entity_references(
    memories: &mut [NewMemory],
    before: &str,
    after: &str,
) {
    let normalized_before = normalize_path(before);
    for memory in memories {
        for entity in &mut memory.entities {
            if entity == before
                || normalized_before
                    .as_ref()
                    .is_some_and(|before| normalize_path(entity).as_ref() == Some(before))
            {
                *entity = after.to_string();
            }
        }
    }
}

pub(super) fn revised_memory_path(
    path: &str,
    previous: &Batch,
    revised: Option<&Batch>,
) -> Option<String> {
    let tail = path.strip_prefix("memories[")?;
    let (index, suffix) = tail.split_once(']')?;
    let index = index.parse::<usize>().ok()?;
    let id = previous.memories.get(index)?.id.trim();
    if id.is_empty() {
        return None;
    }
    let revised_index = revised?
        .memories
        .iter()
        .position(|memory| memory.id == id)?;
    Some(format!("memories[{revised_index}]{suffix}"))
}

pub(super) fn current_memory_path(path: &str, previous: &Batch, current: &Batch) -> Option<String> {
    let tail = path.strip_prefix("memories[")?;
    let (index, suffix) = tail.split_once(']')?;
    let index = index.parse::<usize>().ok()?;
    let id = previous.memories.get(index)?.id.trim();
    if id.is_empty() {
        return None;
    }
    let current_index = current.memories.iter().position(|memory| memory.id == id)?;
    Some(format!("memories[{current_index}]{suffix}"))
}

pub(super) fn merge_research_coverage_revision(base: &mut Batch, revised: &Batch) {
    for memory in &revised.memories {
        if !base.memories.iter().any(|existing| {
            existing.kind == memory.kind
                && existing.content == memory.content
                && existing.entities == memory.entities
                && existing.playbook == memory.playbook
        }) {
            base.memories.push(memory.clone());
        }
    }
    for entity in &revised.new_entities {
        if let Some(existing) = base
            .new_entities
            .iter_mut()
            .find(|existing| existing.path == entity.path)
        {
            for attribute in &entity.candidate_attributes {
                if !existing.candidate_attributes.iter().any(|candidate| {
                    candidate.key == attribute.key && candidate.value == attribute.value
                }) {
                    existing.candidate_attributes.push(attribute.clone());
                }
            }
        } else {
            base.new_entities.push(entity.clone());
        }
    }
    for update in &revised.existing_entity_updates {
        if let Some(existing) = base
            .existing_entity_updates
            .iter_mut()
            .find(|existing| existing.path == update.path)
        {
            for attribute in &update.candidate_attributes {
                if !existing.candidate_attributes.iter().any(|candidate| {
                    candidate.key == attribute.key && candidate.value == attribute.value
                }) {
                    existing.candidate_attributes.push(attribute.clone());
                }
            }
        } else {
            base.existing_entity_updates.push(update.clone());
        }
    }
    for playbook in &revised.playbooks {
        if !base
            .playbooks
            .iter()
            .any(|existing| existing.id == playbook.id || existing.path == playbook.path)
        {
            base.playbooks.push(playbook.clone());
        }
    }
    for disposition in &revised.research_dispositions {
        base.research_dispositions
            .retain(|existing| existing.message != disposition.message);
        base.research_dispositions.push(disposition.clone());
    }
}

pub(super) fn reconcile_failed_attributes<T>(
    previous_pages: &mut [T],
    revised_entities: &[T],
    prefix: &str,
    failures: &[GroundingFailure],
    entity_path: impl Fn(&T) -> &String,
    previous_attributes: impl Fn(&mut T) -> &mut Vec<CandidateAttribute>,
    revised_attributes: impl Fn(&T) -> &Vec<CandidateAttribute>,
) {
    for (page_index, entity) in previous_pages.iter_mut().enumerate() {
        let marker = format!("{prefix}[{page_index}].candidate_attributes[");
        let failed = failures
            .iter()
            .filter_map(|failure| {
                failure
                    .field_path
                    .strip_prefix(&marker)?
                    .split_once(']')?
                    .0
                    .parse::<usize>()
                    .ok()
            })
            .collect::<BTreeSet<_>>();
        if failed.is_empty() {
            continue;
        }
        let current_path = entity_path(entity).clone();
        let mut remaining = revised_entities
            .iter()
            .find(|candidate| entity_path(candidate) == &current_path)
            .map(&revised_attributes)
            .cloned()
            .unwrap_or_default();
        let held = previous_attributes(entity);
        for (index, attribute) in held.iter().enumerate() {
            if failed.contains(&index) {
                continue;
            }
            if let Some(position) = remaining.iter().position(|candidate| {
                (!attribute.id.is_empty() && candidate.id == attribute.id)
                    || (candidate.key == attribute.key
                        && candidate.value == attribute.value
                        && candidate
                            .sources
                            .iter()
                            .map(|source| (&source.message, &source.quote))
                            .eq(attribute
                                .sources
                                .iter()
                                .map(|source| (&source.message, &source.quote))))
            }) {
                remaining.remove(position);
            }
        }
        *held = held
            .iter()
            .enumerate()
            .filter_map(|(index, attribute)| {
                if !failed.contains(&index) {
                    return Some(attribute.clone());
                }
                remaining
                    .iter()
                    .position(|candidate| {
                        (!attribute.id.is_empty() && candidate.id == attribute.id)
                            || plausible_attribute_revision(attribute, candidate)
                    })
                    .map(|position| remaining.remove(position))
            })
            .collect();
    }
}

pub(super) fn plausible_attribute_revision(
    previous: &CandidateAttribute,
    revised: &CandidateAttribute,
) -> bool {
    previous.key == revised.key
        || previous.sources.iter().any(|before| {
            revised
                .sources
                .iter()
                .any(|after| before.message == after.message)
        })
}

pub(super) fn plausible_memory_revision(previous: &NewMemory, revised: &NewMemory) -> bool {
    let same_source = previous.sources.iter().any(|before| {
        revised
            .sources
            .iter()
            .any(|after| before.message == after.message)
    });
    let same_page = previous
        .entities
        .iter()
        .any(|before| revised.entities.iter().any(|after| before == after));
    same_source && same_page
}

pub(super) fn field_path_pointer(path: &str) -> String {
    let mut pointer = String::new();
    let mut segment = String::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '.' => {
                if !segment.is_empty() {
                    pointer.push('/');
                    pointer.push_str(&segment);
                    segment.clear();
                }
            }
            '[' => {
                if !segment.is_empty() {
                    pointer.push('/');
                    pointer.push_str(&segment);
                    segment.clear();
                }
                let mut index = String::new();
                for next in chars.by_ref() {
                    if next == ']' {
                        break;
                    }
                    index.push(next);
                }
                pointer.push('/');
                pointer.push_str(&index);
            }
            _ => segment.push(ch),
        }
    }
    if !segment.is_empty() {
        pointer.push('/');
        pointer.push_str(&segment);
    }
    pointer
}
