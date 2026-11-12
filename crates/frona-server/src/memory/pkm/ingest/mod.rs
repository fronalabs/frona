//! Prepare an extraction snapshot and translate the portable phase output for persistence.
use std::sync::Arc;

pub(crate) mod transcript;
use super::consolidation::ConsolidationContext;
use crate::core::error::AppError;
use crate::db::repo::pkm::{IngestBatch, PendingEntity, PendingEntityUpdate, PendingMemory};
use crate::memory::pkm::model::PendingPlaybookCandidate;
use frona_pkm::ingest::{EntitySummary, IngestInput, IngestPrompts};

pub struct Ingest {
    ctx: Arc<ConsolidationContext>,
}
impl Ingest {
    pub fn new(ctx: Arc<ConsolidationContext>) -> Self {
        Self { ctx }
    }

    pub async fn run(&self, transcript: &str) -> Result<IngestBatch, AppError> {
        if transcript.trim().is_empty() {
            return Ok(IngestBatch::default());
        }
        let scope = &self.ctx.scope;
        let self_path = self
            .ctx
            .repo
            .ensure_self_entity(&scope.user_id, &scope.user_name)
            .await
            .unwrap_or_else(|_| crate::memory::pkm::model::SELF_ENTITY_PATH.to_string());
        let existing_entities = self
            .ctx
            .repo
            .list_entities(&scope.user_id)
            .await?
            .into_iter()
            .map(|entity| EntitySummary {
                path: entity.path,
                name: entity.name,
            })
            .collect();
        let input = IngestInput {
            transcript: transcript.into(),
            owner_name: scope.user_name.clone(),
            handle: scope.vault.handle().as_str().into(),
            self_path,
            existing_entities,
            evidence_sources: scope.evidence_sources.clone(),
            temporal_sources: scope.temporal_sources.clone(),
            recall: scope.recall.clone(),
            timezone: scope.timezone.clone(),
            erroneous_contents: self
                .ctx
                .repo
                .erroneous_contents_snapshot(&scope.user_id)
                .await?,
        };
        let mut prompts = IngestPrompts::default();
        prompts.defaults = self.ctx.llm.prompt_defaults();
        prompts.system = self
            .ctx
            .llm
            .prompt_source("pkm/ingest/system.md", &prompts.system)?;
        prompts.input = self
            .ctx
            .llm
            .prompt_source("pkm/ingest/input.md", &prompts.input)?;
        prompts.reject = self
            .ctx
            .llm
            .prompt_source("pkm/ingest/reject.md", &prompts.reject)?;
        let batch = frona_pkm::ingest::Ingest::new(self.ctx.llm.harness())
            .prompts(prompts)
            .run_with(input, |run| self.ctx.llm.configure(run))
            .await?;
        Ok(persistence_batch(batch))
    }
}

fn persistence_batch(batch: frona_pkm::ingest::IngestBatch) -> IngestBatch {
    IngestBatch {
        entities: batch
            .entities
            .into_iter()
            .map(|entity| PendingEntity {
                path: entity.path,
                name: entity.name,
                description: entity.description,
                aliases: entity.aliases,
                attributes: entity.attributes,
                identity_evidence: entity
                    .identity_evidence
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                attribute_evidence: entity
                    .attribute_evidence
                    .into_iter()
                    .map(|(key, evidence)| (key, evidence.into_iter().map(Into::into).collect()))
                    .collect(),
            })
            .collect(),
        entity_updates: batch
            .entity_updates
            .into_iter()
            .map(|entity| PendingEntityUpdate {
                path: entity.path,
                attributes: entity.attributes,
                attribute_evidence: entity
                    .attribute_evidence
                    .into_iter()
                    .map(|(key, evidence)| (key, evidence.into_iter().map(Into::into).collect()))
                    .collect(),
            })
            .collect(),
        memories: batch
            .memories
            .into_iter()
            .map(|memory| PendingMemory {
                id: memory.id,
                kind: memory.kind.into(),
                evidence: memory.evidence.into_iter().map(Into::into).collect(),
                episode: memory.episode.map(Into::into),
                content: memory.content,
                paths: memory.paths,
            })
            .collect(),
        playbook_candidates: batch
            .playbook_candidates
            .into_iter()
            .map(|candidate| PendingPlaybookCandidate {
                id: candidate.id,
                path: candidate.path,
                name: candidate.name,
                description: candidate.description,
                source_memory_ids: candidate.source_memory_ids,
            })
            .collect(),
        grounding_corrections: batch.grounding_corrections,
        grounding_items_dropped: batch.grounding_items_dropped,
        recall_result_lookups: batch.recall_result_lookups,
        agent_evidence_no_tool_drops: batch.agent_evidence_no_tool_drops,
        agent_evidence_strong_matches: batch.agent_evidence_strong_matches,
        agent_evidence_fallback_reviews: batch.agent_evidence_fallback_reviews,
        agent_evidence_fallback_retains: batch.agent_evidence_fallback_retains,
        agent_evidence_invalid_submissions: batch.agent_evidence_invalid_submissions,
        agent_evidence_lookup_calls: batch.agent_evidence_lookup_calls,
        agent_evidence_terminal_drops: batch.agent_evidence_terminal_drops,
        research_coverage: batch.research_coverage.into(),
    }
}
