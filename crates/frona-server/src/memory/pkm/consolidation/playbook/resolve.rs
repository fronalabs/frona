use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use frona_pkm::playbook::resolve::ResolvedPlaybook;

use crate::core::error::AppError;
use crate::db::repo::pkm::PlaybookResolutionWrite;
use crate::memory::pkm::graph::EntityTransition;
use crate::memory::pkm::model::{
    ConsolidationEntityLifecycle, EntityCategory, KnowledgeConsolidationEntity, PLAYBOOK_KIND_IRI,
    PlaybookResolutionProgress,
};

use crate::memory::pkm::consolidation::ConsolidationStageState;
use crate::memory::pkm::consolidation::context::ConsolidationContext;
use crate::memory::pkm::model::PendingPlaybookCandidate;

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
        let memory_ids = candidate
            .source_memory_ids
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let memories = self
            .ctx
            .repo
            .memories_by_ids(&self.ctx.scope.user_id, &memory_ids)
            .await?;
        let staged_paths = staged
            .iter()
            .map(|target| target.path.clone())
            .collect::<BTreeSet<_>>();
        let entities = self
            .ctx
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
            .map(|row| row.as_knowledge_entity())
            .collect();
        let knowledge = super::snapshot::load(
            &self.ctx,
            entities,
            &memories,
            &self.messages,
            &self.tool_calls,
        )
        .await?;
        let input = frona_pkm::playbook::resolve::PlaybookResolveInput {
            candidate: frona_pkm::ingest::PendingPlaybookCandidate {
                id: candidate.id.clone(),
                path: candidate.path.clone(),
                name: candidate.name.clone(),
                description: candidate.description.clone(),
                source_memory_ids: candidate.source_memory_ids.clone(),
            },
            memories: memories.into_iter().map(Into::into).collect(),
            knowledge,
            staged_paths,
        };
        let mut prompts = frona_pkm::playbook::resolve::PlaybookResolvePrompts::default();
        prompts.defaults = self.ctx.llm.prompt_defaults();
        prompts.system = self
            .ctx
            .llm
            .prompt_source("pkm/playbook-resolve/system.md", &prompts.system)?;
        prompts.input = self
            .ctx
            .llm
            .prompt_source("pkm/playbook-resolve/input.md", &prompts.input)?;
        prompts.reject = self
            .ctx
            .llm
            .prompt_source("pkm/playbook-resolve/reject.md", &prompts.reject)?;
        Ok(
            frona_pkm::playbook::resolve::PlaybookResolve::new(self.ctx.llm.harness())
                .prompts(prompts)
                .exploration_turns(self.max_tool_turns)
                .submission_attempts(self.max_submissions)
                .run_with(input, |run| self.ctx.llm.configure(run))
                .await?,
        )
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

fn coalesced_targets(
    proposals: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<ResolvedPlaybook>, AppError> {
    let proposals = proposals
        .iter()
        .map(|(id, value)| {
            serde_json::from_value(value.clone())
                .map(|targets| (id.clone(), targets))
                .map_err(|error| AppError::Internal(format!("playbook resolve decode: {error}")))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(frona_pkm::playbook::resolve::coalesced_targets(&proposals))
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
