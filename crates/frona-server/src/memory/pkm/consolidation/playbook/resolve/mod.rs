use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

mod inspection;
mod state;
use frona_harness::{FailureKind, RunOutcome};
use inspection::PlaybookInspection;

pub use state::PlaybookResolveState;

use serde::{Deserialize, Serialize};

use crate::core::error::AppError;
use crate::db::repo::pkm::PlaybookResolutionWrite;
use crate::memory::pkm::consolidation::candidates::{
    RESOLUTION_PROMPT_LIMIT, Request, Search, Subject,
};
use crate::memory::pkm::consolidation::view::EntityTransition;
use crate::memory::pkm::model::{
    ConsolidationEntityLifecycle, EntityCategory, EntityHit, KnowledgeConsolidationEntity,
    PLAYBOOK_KIND_IRI, PlaybookResolutionProgress,
};
use crate::memory::pkm::storage::normalize_path;
use crate::tool::AgentTool;
use crate::tool::registry::ToolFilter;

use crate::memory::pkm::consolidation::context::ConsolidationContext;
use crate::memory::pkm::consolidation::{ConsolidationStageState, PromptIds, PromptSpec};
use crate::memory::pkm::model::PendingPlaybookCandidate;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ResolvedPlaybook {
    pub existing_path: Option<String>,
    #[serde(default)]
    pub merge_from: Vec<String>,
    pub path: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub memory_ids: Vec<String>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct Verdict {
    #[serde(default)]
    playbooks: Vec<ResolvedPlaybook>,
}

pub(crate) struct PlaybookResolve {
    pub(crate) ctx: Arc<ConsolidationContext>,
    pub(crate) messages: crate::db::repo::messages::SurrealMessageRepo,
    pub(crate) tool_calls: Arc<crate::db::repo::tool_calls::SurrealToolCallRepo>,
    pub(crate) max_tool_turns: usize,
    pub(crate) max_submissions: usize,
}

impl PlaybookResolve {
    pub(crate) async fn run(&self) -> Result<(), AppError> {
        let mut state = match self.ctx.stage().await {
            ConsolidationStageState::PlaybookResolve(state) => state,
            other => {
                return Err(AppError::Internal(format!(
                    "playbook resolve: record is on `{}`",
                    other.label()
                )));
            }
        };
        let mut rows: BTreeMap<String, KnowledgeConsolidationEntity> = self
            .ctx
            .view
            .rows()
            .await?
            .into_iter()
            .filter(|row| row.category == EntityCategory::Playbook)
            .map(|row| (row.consolidation_entity_id.clone(), row))
            .collect();
        let candidates: BTreeMap<String, PendingPlaybookCandidate> = rows
            .values()
            .filter(|row| !row.contributions.is_empty())
            .map(|row| {
                (
                    row.consolidation_entity_id.clone(),
                    PendingPlaybookCandidate {
                        id: row.consolidation_entity_id.clone(),
                        path: row.path.clone(),
                        name: row.name.clone(),
                        description: row.description.clone(),
                        source_memory_ids: row.source_memory_ids.iter().cloned().collect(),
                    },
                )
            })
            .collect();
        let mut proposals: BTreeMap<String, serde_json::Value> =
            rows.iter()
                .filter_map(|(id, row)| match &row.progress.playbook_resolution {
                    PlaybookResolutionProgress::Accepted { proposal } => {
                        Some((id.clone(), proposal.clone()))
                    }
                    PlaybookResolutionProgress::Pending
                    | PlaybookResolutionProgress::Failed { .. } => None,
                })
                .collect();

        for (id, candidate) in &candidates {
            if proposals.contains_key(id) {
                continue;
            }
            let staged = coalesced_targets(&proposals)?;
            match self.resolve_one(candidate, &staged).await {
                Ok(targets) => {
                    let proposal = serde_json::to_value(&targets)
                        .map_err(|e| AppError::Internal(format!("playbook resolve encode: {e}")))?;
                    state.revision += 1;
                    let mut row = rows.get(id).cloned().ok_or_else(|| {
                        AppError::Internal(format!("playbook resolve: missing row {id}"))
                    })?;
                    row.progress.playbook_resolution = PlaybookResolutionProgress::Accepted {
                        proposal: proposal.clone(),
                    };
                    row.checkpoint_revision = state.revision;
                    let projection_rows = self
                        .accepted_projection_rows(row, &targets, &rows, state.revision)
                        .await?;
                    let mut checkpoint = self.ctx.record().await;
                    checkpoint.state = ConsolidationStageState::PlaybookResolve(state.clone());
                    checkpoint.updated_at = chrono::Utc::now();
                    let transition =
                        EntityTransition::new(checkpoint.clone()).with_rows(projection_rows);
                    self.ctx.view.commit_transition(&transition).await?;
                    self.ctx.adopt_committed_record(checkpoint).await;
                    rows = self
                        .ctx
                        .view
                        .rows()
                        .await?
                        .into_iter()
                        .filter(|row| row.category == EntityCategory::Playbook)
                        .map(|row| (row.consolidation_entity_id.clone(), row))
                        .collect();
                    proposals.insert(id.clone(), proposal);
                }
                Err(error) => {
                    state.revision += 1;
                    let mut row = rows.get(id).cloned().ok_or_else(|| {
                        AppError::Internal(format!("playbook resolve: missing row {id}"))
                    })?;
                    row.progress.playbook_resolution = PlaybookResolutionProgress::Failed {
                        error: error.to_string(),
                    };
                    row.checkpoint_revision = state.revision;
                    let mut checkpoint = self.ctx.record().await;
                    checkpoint.state = ConsolidationStageState::PlaybookResolve(state.clone());
                    checkpoint.updated_at = chrono::Utc::now();
                    let transition = EntityTransition::new(checkpoint.clone()).with_row(row);
                    self.ctx.view.commit_transition(&transition).await?;
                    self.ctx.adopt_committed_record(checkpoint).await;
                    return Err(error);
                }
            }
        }

        self.apply(&candidates, &proposals).await?;
        Ok(())
    }

    async fn accepted_projection_rows(
        &self,
        owner: KnowledgeConsolidationEntity,
        targets: &[ResolvedPlaybook],
        known_rows: &BTreeMap<String, KnowledgeConsolidationEntity>,
        revision: u64,
    ) -> Result<Vec<KnowledgeConsolidationEntity>, AppError> {
        let known_by_path: BTreeMap<_, _> = known_rows
            .values()
            .map(|row| (row.path.clone(), row.clone()))
            .collect();
        let mut changed = BTreeMap::<String, KnowledgeConsolidationEntity>::new();
        changed.insert(owner.path.clone(), owner.clone());

        for target in targets {
            let mut target_row = if owner.path == target.path {
                owner.clone()
            } else if let Some(row) = known_by_path.get(&target.path) {
                row.clone()
            } else if let Some(row) = self
                .ctx
                .view
                .entity_by_path(&target.path)
                .await?
                .filter(|row| row.path == target.path)
            {
                row
            } else {
                KnowledgeConsolidationEntity::pending(
                    self.ctx.view.consolidation_id(),
                    &self.ctx.scope.user_id,
                    &target.path,
                    EntityCategory::Playbook,
                    Vec::new(),
                    target.memory_ids.iter().cloned().collect(),
                )
            };
            target_row.category = EntityCategory::Playbook;
            target_row.lifecycle = ConsolidationEntityLifecycle::Active;
            target_row.canonical_path = None;
            target_row.kinds = vec![PLAYBOOK_KIND_IRI.to_string()];
            target_row.name = target.name.clone();
            target_row.description = target.description.clone();
            target_row
                .source_memory_ids
                .extend(target.memory_ids.iter().cloned());
            target_row.source_memory_ids.sort();
            target_row.source_memory_ids.dedup();
            target_row.checkpoint_revision = revision;
            target_row.rederive_search();
            changed.insert(target.path.clone(), target_row);

            let mut sources = target.merge_from.clone();
            if let Some(source) = target
                .existing_path
                .as_ref()
                .filter(|path| *path != &target.path)
            {
                sources.push(source.clone());
            }
            sources.sort();
            sources.dedup();
            for source in sources {
                let mut source_row = if source == owner.path {
                    owner.clone()
                } else if let Some(row) = known_by_path.get(&source) {
                    row.clone()
                } else if let Some(row) = self
                    .ctx
                    .view
                    .entity_by_path(&source)
                    .await?
                    .filter(|row| row.path == source)
                {
                    row
                } else {
                    continue;
                };
                source_row.mark_coalesced(&target.path);
                source_row.checkpoint_revision = revision;
                changed.insert(source, source_row);
            }
        }

        if !targets.iter().any(|target| target.path == owner.path)
            && !changed
                .get(&owner.path)
                .is_some_and(|row| row.lifecycle == ConsolidationEntityLifecycle::Coalesced)
        {
            let mut owner_row = owner;
            if targets.len() == 1 {
                owner_row.mark_coalesced(&targets[0].path);
            } else {
                owner_row.lifecycle = ConsolidationEntityLifecycle::Discarded;
                owner_row.searchable = false;
                owner_row.canonical_path = None;
            }
            owner_row.checkpoint_revision = revision;
            changed.insert(owner_row.path.clone(), owner_row);
        }

        Ok(changed.into_values().collect())
    }

    async fn resolve_one(
        &self,
        candidate: &PendingPlaybookCandidate,
        staged: &[ResolvedPlaybook],
    ) -> Result<Vec<ResolvedPlaybook>, AppError> {
        let memory_ids: Vec<String> = candidate.source_memory_ids.iter().cloned().collect();
        let memories = self
            .ctx
            .repo
            .memories_by_ids(&self.ctx.scope.user_id, &memory_ids)
            .await?;
        let ids = PromptIds::new("m", memories.iter().map(|memory| memory.id.clone()));
        let memory_block = memories
            .iter()
            .map(|memory| format!("- {}: {}", ids.local(&memory.id), memory.content))
            .collect::<Vec<_>>()
            .join("\n");

        let staged_paths = staged
            .iter()
            .map(|entity| entity.path.clone())
            .collect::<BTreeSet<_>>();
        let mut visible_entities: BTreeMap<String, crate::memory::pkm::model::KnowledgeEntity> =
            self.ctx
                .view
                .list_entities()
                .await?
                .into_iter()
                .filter(|row| {
                    row.entity_id.is_some()
                        || row.contributions.is_empty()
                        || staged_paths.contains(&row.path)
                        || matches!(
                            row.progress.playbook_resolution,
                            PlaybookResolutionProgress::Accepted { .. }
                        )
                })
                .map(|row| (row.path.clone(), row.as_knowledge_entity()))
                .collect();
        let existing_by_path: BTreeMap<String, EntityCategory> = visible_entities
            .values()
            .map(|entity| (entity.path.clone(), entity.category))
            .collect();
        if let Some(entity) = visible_entities.get_mut(&candidate.path) {
            entity.category = EntityCategory::Playbook;
            entity.kinds = vec![PLAYBOOK_KIND_IRI.to_string()];
            entity.name = candidate.name.clone();
            entity.description = candidate.description.clone();
            entity.source_memory_ids.extend(memory_ids.iter().cloned());
            entity.source_memory_ids.sort();
            entity.source_memory_ids.dedup();
        } else {
            let mut entity = KnowledgeConsolidationEntity::pending(
                self.ctx.view.consolidation_id(),
                &self.ctx.scope.user_id,
                &candidate.path,
                EntityCategory::Playbook,
                Vec::new(),
                memory_ids.iter().cloned().collect(),
            );
            entity.kinds = vec![PLAYBOOK_KIND_IRI.to_string()];
            entity.name = candidate.name.clone();
            entity.description = candidate.description.clone();
            visible_entities.insert(candidate.path.clone(), entity.as_knowledge_entity());
        }
        let entity_view = Arc::new(
            crate::memory::pkm::consolidation::tools::EntityToolView::new(
                visible_entities.into_values().collect(),
                usize::MAX,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            ),
        );
        let eligible_paths: HashSet<String> = entity_view
            .entities()
            .iter()
            .filter(|entity| entity.category == EntityCategory::Playbook)
            .map(|entity| entity.path.clone())
            .collect();
        let view_hits: Vec<EntityHit> = entity_view
            .entities()
            .iter()
            .map(|entity| EntityHit {
                path: entity.path.clone(),
                origin: entity.origin,
                category: entity.category,
                kinds: entity.kinds.clone(),
                name: entity.name.clone(),
                description: entity.description.clone(),
                aliases: entity.aliases.clone(),
                body: entity.body.clone(),
                search_name_tokens: entity.search_name_tokens.clone(),
                search_assertions: entity.search_assertions.clone(),
            })
            .collect();
        let subject = Subject::from_parts(
            candidate.path.clone(),
            candidate.name.clone(),
            std::iter::empty(),
            candidate.description.clone(),
            EntityCategory::Playbook,
            vec![PLAYBOOK_KIND_IRI.to_string()],
            std::iter::empty(),
        );
        let existing = Search::new(self.ctx.view.clone())
            .find_candidates(
                Request {
                    subject,
                    eligible_paths: Some(eligible_paths.clone()),
                    additional_candidates: view_hits,
                    forced_paths: Vec::new(),
                    limit: RESOLUTION_PROMPT_LIMIT,
                },
                |entity| entity.kinds = vec![PLAYBOOK_KIND_IRI.to_string()],
                |_, _| Some(3),
            )
            .await?;
        let existing_lines = existing.iter().map(|candidate| format!(
            "- path={} name={} description={} exact_name={} token_containment={} similarity={:.3}",
            candidate.entity.path, candidate.entity.name, candidate.entity.description,
            candidate.evidence.exact_name, candidate.evidence.token_containment,
            candidate.evidence.ordered_similarity.max(candidate.evidence.token_order_similarity),
        )).collect::<Vec<_>>();
        let existing_block = if existing_lines.is_empty() {
            "(none)".to_string()
        } else {
            existing_lines.join("\n")
        };
        let candidate_block = format!(
            "path={}\nname={}\ndescription={}",
            candidate.path, candidate.name, candidate.description
        );
        let vars = [
            ("candidate", candidate_block.as_str()),
            ("memories", memory_block.as_str()),
            ("existing_playbooks", existing_block.as_str()),
        ];

        let allowed: BTreeSet<String> = memories.iter().map(|memory| memory.id.clone()).collect();
        let playbook_lookup = Arc::new(super::tools::PlaybookLookup::default());
        let total_tool_budget = Arc::new(std::sync::atomic::AtomicUsize::new(20));
        let memory_lookup = memories
            .iter()
            .map(|memory| (ids.local(&memory.id).to_string(), memory.clone()))
            .collect();
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(super::tools::ReadMemoryContextTool {
                prompts: self.ctx.llm.prompts().clone(),
                memories: memory_lookup,
                messages: self.messages.clone(),
                tool_calls: self.tool_calls.clone(),
                total: total_tool_budget.clone(),
            }),
            Arc::new(super::tools::FindPlaybooksTool {
                prompts: self.ctx.llm.prompts().clone(),
                repo: self.ctx.view.clone(),
                view: Some(entity_view.clone()),
                eligible_playbook_paths: Some(Arc::new(eligible_paths)),
                subject_path: Some(candidate.path.clone()),
                total: total_tool_budget.clone(),
            }),
            Arc::new(super::tools::ReadPlaybookTool {
                prompts: self.ctx.llm.prompts().clone(),
                repo: self.ctx.view.clone(),
                memories: self.ctx.repo.clone(),
                user_id: self.ctx.scope.user_id.clone(),
                view: Some(entity_view),
                lookup: playbook_lookup.clone(),
                total: total_tool_budget,
            }),
        ];
        let harness = self
            .ctx
            .llm
            .sdk(
                &self.ctx.scope.agent_id,
                &[ToolFilter::AllowList(&[
                    "read_memory_context",
                    "find_playbooks",
                    "read_playbook",
                ])],
                &tools,
            )
            .await?;
        let inspection = Arc::new(PlaybookInspection {
            ctx: self.ctx.clone(),
            ids,
            allowed,
            existing_by_path,
            playbook_lookup,
            staged_paths,
        });
        let result = harness
            .run(PromptSpec::PLAYBOOK_RESOLVE, &vars)?
            .exploration_turns(self.max_tool_turns)
            .structured::<Verdict>()
            .submission_attempts(self.max_submissions)
            .inspect(move |verdict| {
                let inspection = inspection.clone();
                async move { inspection.inspect(verdict) }
            })
            .await;
        match result {
            Ok(accepted) => Ok(accepted),
            Err(incomplete) => {
                tracing::warn!(reason = %incomplete.reason, outcome = ?incomplete.report.outcome,
                    "playbook resolution incomplete");
                if matches!(
                    incomplete.report.outcome,
                    Some(RunOutcome::Incomplete {
                        kind: FailureKind::Cancelled,
                        ..
                    })
                ) {
                    return Err(AppError::Inference(Box::new(
                        frona_harness::inference::InferenceError::Cancelled(
                            incomplete.reason.to_string(),
                        ),
                    )));
                }
                // The outer work queue checkpoints Failed and retries this item.
                // An unfinished assignment must never become an accepted empty plan.
                Err(AppError::Internal(format!(
                    "playbook resolution incomplete: {}",
                    incomplete.reason
                )))
            }
        }
    }

    async fn apply(
        &self,
        candidates_by_id: &BTreeMap<String, PendingPlaybookCandidate>,
        proposals: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), AppError> {
        let writes = coalesced_targets(proposals)?
            .into_iter()
            .map(|target| {
                let assigned: BTreeSet<_> = target.memory_ids.iter().collect();
                let candidates = candidates_by_id
                    .iter()
                    .filter(|(_, candidate)| {
                        candidate
                            .source_memory_ids
                            .iter()
                            .any(|memory| assigned.contains(memory))
                    })
                    .collect::<Vec<_>>();
                PlaybookResolutionWrite {
                    candidate_ids: candidates.iter().map(|(id, _)| (*id).clone()).collect(),
                    candidate_paths: candidates
                        .iter()
                        .map(|(_, candidate)| candidate.path.clone())
                        .collect(),
                    existing_path: target.existing_path,
                    merge_from: target.merge_from,
                    path: target.path,
                    name: target.name,
                    description: target.description,
                    memory_ids: target.memory_ids,
                }
            })
            .collect::<Vec<_>>();
        let mut completed = self.ctx.record().await;
        completed.state = completed.state.next();
        completed.attempts = 0;
        completed.updated_at = chrono::Utc::now();
        self.ctx
            .repo
            .commit_playbook_resolutions(&self.ctx.scope.user_id, &writes, &completed)
            .await?;
        self.ctx.adopt_committed_record(completed).await;
        Ok(())
    }
}

/// Fold serially accepted candidate projections into the view seen by the next
/// conversation and, eventually, the atomic commit. A later decision for the same path
/// owns its expanded scope/name/description, while every earlier memory assignment is
/// retained. Keeping the first metadata made a successful scope expansion invisible.
fn coalesced_targets(
    proposals: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<ResolvedPlaybook>, AppError> {
    let mut targets = BTreeMap::<String, ResolvedPlaybook>::new();
    for proposal in proposals.values() {
        let parsed: Vec<ResolvedPlaybook> = serde_json::from_value(proposal.clone())
            .map_err(|error| AppError::Internal(format!("playbook resolve decode: {error}")))?;
        for mut target in parsed {
            if let Some(earlier) = targets.remove(&target.path) {
                target.memory_ids.extend(earlier.memory_ids);
                target.memory_ids.sort();
                target.memory_ids.dedup();
                target.merge_from.extend(earlier.merge_from);
                target.merge_from.sort();
                target.merge_from.dedup();
                if target.existing_path.is_none() {
                    target.existing_path = earlier.existing_path;
                }
            }
            targets.insert(target.path.clone(), target);
        }
    }
    Ok(targets.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_target_keeps_all_memories_and_latest_scope() {
        let mut proposals = BTreeMap::new();
        proposals.insert(
            "candidate-1".to_string(),
            serde_json::json!([{
                "path": "operations/update-device",
                "name": "Enter update mode",
                "description": "Put a device into its firmware update mode.",
                "memory_ids": ["memory-1"]
            }]),
        );
        proposals.insert(
            "candidate-2".to_string(),
            serde_json::json!([{
                "existing_path": "operations/update-device",
                "path": "operations/update-device",
                "name": "Update device firmware",
                "description": "Enter update mode, install firmware, and verify the result.",
                "memory_ids": ["memory-2"]
            }]),
        );

        let targets = coalesced_targets(&proposals).expect("proposals should coalesce");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "Update device firmware");
        assert_eq!(
            targets[0].description,
            "Enter update mode, install firmware, and verify the result."
        );
        assert_eq!(targets[0].memory_ids, ["memory-1", "memory-2"]);
        assert_eq!(
            targets[0].existing_path.as_deref(),
            Some("operations/update-device")
        );
    }
}
