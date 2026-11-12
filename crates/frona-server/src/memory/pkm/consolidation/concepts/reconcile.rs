//! Reconciliation worklist and durable application of portable phase results.
use super::{
    progress::ConsolidationProgress, resolve::ResolveWorkState,
    validation::validate_proposal_projection,
};
use crate::core::error::AppError;
use crate::db::repo::pkm::{
    ReconcileEntityLinkSourceWrite, ReconcileMemoryRelationWrite, ReconcileOutdatedWrite,
};
use crate::memory::pkm::{
    consolidation::{Consolidator, ReconcileOutcome, ResolveOutcome},
    model::{EntityCategory, SELF_ENTITY_PATH},
};
use crate::{
    db::repo::pkm::ReconcileCommit, memory::pkm::consolidation::context::ConsolidationContext,
};
use frona_pkm::projection::projection_rejection_details;
use frona_pkm::reconcile::ReconcilePromotion;
use frona_pkm::{
    classify::ProposalSet,
    reconcile::{ReconcileInput, ReconcilePrompts},
};
use std::{collections::HashSet, sync::Arc};
use tracing::warn;
#[derive(Clone)]
pub(super) struct Reconcile {
    pub ctx: Arc<ConsolidationContext>,
    pub ontology: crate::memory::pkm::ontology::OntologyManager,
    pub users: crate::auth::user_service::UserService,
    pub prefixes: frona_pkm::ontology::PrefixMap,
    pub max_submissions: usize,
}
#[derive(Default)]
struct EntityOutcome {
    reconcile_dirty: HashSet<String>,
    promotions: Vec<ReconcilePromotion>,
    retractions: Vec<(String, String)>,
    proposals: frona_pkm::classify::ProposalSet,
    commit: ReconcileCommit,
    profile_attributes: Option<serde_json::Value>,
}

impl EntityOutcome {
    fn from_phase(
        path: &str,
        mut output: frona_pkm::reconcile::ReconcileOutput,
    ) -> Result<Self, AppError> {
        let entity = if output.entity_update.is_some() {
            let mut row = output.proposals.input_entity(path).ok_or_else(|| {
                AppError::Internal(format!(
                    "reconcile: accepted entity `{path}` missing from phase output"
                ))
            })?;
            row.lifecycle = frona_pkm::model::ConsolidationEntityLifecycle::Active;
            row.rederive_search();
            row.validate()?;
            output.proposals.stage_input_entity(row.clone());
            Some(row.into())
        } else {
            None
        };
        Ok(Self {
            reconcile_dirty: output.reconcile_dirty,
            promotions: output.promotions,
            retractions: output.retractions,
            proposals: output.proposals,
            profile_attributes: output.profile_attributes,
            commit: ReconcileCommit {
                entity,
                memory_relations: output
                    .changes
                    .memory_relations
                    .into_iter()
                    .map(|change| ReconcileMemoryRelationWrite {
                        subordinate_id: change.subordinate_id,
                        relation: change.relation.into(),
                        to_id: change.to_id,
                        note: change.note,
                    })
                    .collect(),
                outdated_memories: output
                    .changes
                    .outdated_memories
                    .into_iter()
                    .map(|change| ReconcileOutdatedWrite {
                        memory_id: change.memory_id,
                        reason: change.reason,
                    })
                    .collect(),
                entity_link_sources: output
                    .changes
                    .entity_link_sources
                    .into_iter()
                    .map(|change| ReconcileEntityLinkSourceWrite {
                        from_entity_path: change.from_entity_path,
                        to_entity_path: change.to_entity_path,
                        relation: change.relation,
                        source_memory_ids: change.source_memory_ids,
                    })
                    .collect(),
            },
        })
    }
}

struct ReconciliationEffects<'a> {
    path: &'a str,
    promotions: &'a [ReconcilePromotion],
    retractions: &'a [(String, String)],
}

struct ReconcileResolutionState<'a> {
    resolve: &'a mut ResolveWorkState,
    ontology_stats: &'a mut ResolveOutcome,
    worklist: &'a mut ReconcileWorklist,
}

struct ReconcileWorklist {
    pending: Vec<String>,
    queued: HashSet<String>,
}

impl ReconcileWorklist {
    fn new(mut pending: Vec<String>) -> Self {
        pending.sort();
        pending.dedup();
        let queued = pending.iter().cloned().collect();
        Self { pending, queued }
    }

    fn pop(&mut self) -> Option<String> {
        let path = self.pending.pop()?;
        self.queued.remove(&path);
        Some(path)
    }

    fn push(&mut self, path: String) -> bool {
        if !self.queued.insert(path.clone()) {
            return false;
        }
        self.pending.push(path);
        true
    }
}

impl Reconcile {
    /// Reconcile every changed concept entity as a **worklist drain**: seed with the
    /// entities needing reconciliation; each entity emits global relations that can retire
    /// memories shared with other entities, and a retirement that unions a survivor onto
    /// a new entity re-enqueues it. Terminates without a cap: an entity is re-enqueued only
    /// when a memory was retired (a finite, one-way budget).
    ///
    /// Counts are banked **per entity** rather than returned: a resumed stage only sees the
    /// entities it still owes, so a total accumulated here would silently drop everything
    /// finished before the crash.
    ///
    /// Nothing is handed to author either - every write that changes what an entity renders
    /// bumps its `updated_at`, so the dirty set is derivable and author re-reads it.
    pub(crate) async fn run(
        &self,
        proposals: &mut ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        resolver: &Consolidator,
        resolve_state: &mut ResolveWorkState,
        ontology_stats: &mut ResolveOutcome,
    ) -> Result<(), AppError> {
        let mut pending = self
            .ctx
            .repo
            .entities_needing_reconciliation(&self.ctx.scope.user_id)
            .await?;
        pending.extend(proposals.input_paths().cloned());
        let mut worklist = ReconcileWorklist::new(pending);
        while let Some(path) = worklist.pop() {
            if resolve_state.is_merged(&path) || progress.canonical_path(&path) != path {
                continue;
            }
            // Reconciled by an earlier attempt at this pass. Nothing in the live tables
            // says so - the completion stamp belongs to author - so the record is the
            // only thing standing between a resume and a second conversation per entity.
            let mut replay_rejection = None;
            if let Some(reconciliation) = progress.reconciliation(&path) {
                let promotions = reconciliation.promotions().to_vec();
                let retractions = reconciliation.retractions().to_vec();
                let mut trial = proposals.clone();
                trial.add_reconcile_promotions(&path, &promotions, &retractions, &self.prefixes);
                if let Some(entity) = self.ctx.view.entity_by_path(&path).await? {
                    trial.apply_reconciled_entity(
                        &path,
                        entity.name,
                        entity.description,
                        entity.attributes.clone(),
                        entity
                            .attribute_sources
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                    );
                    trial.add_reconcile_attributes(&path, &entity.attributes, &[], &self.prefixes);
                }
                let validation =
                    validate_proposal_projection(&self.ctx, &self.ontology, &trial).await?;
                if validation.is_valid() {
                    *proposals = trial;
                    progress
                        .checkpoint_transition("reconcile-replay", &path, proposals)
                        .await?;
                    self.resolve_after_reconcile(
                        ReconciliationEffects {
                            path: &path,
                            promotions: &promotions,
                            retractions: &retractions,
                        },
                        proposals,
                        progress,
                        resolver,
                        ReconcileResolutionState {
                            resolve: resolve_state,
                            ontology_stats,
                            worklist: &mut worklist,
                        },
                    )
                    .await?;
                    continue;
                }
                replay_rejection = Some(projection_rejection_details(&validation));
                progress.reopen_reconciliation(&path);
                progress
                    .checkpoint_transition("reconcile-replay-rejected", &path, proposals)
                    .await?;
            }
            let draft = proposals.entity_draft();
            let Some(entity) = self.ctx.view.entity_by_path_with(&draft, &path).await? else {
                continue;
            };
            if entity.category != EntityCategory::Concept {
                continue;
            }
            // A referenced extraction shell is a typed A-box individual, but has no
            // memories whose state can be reconciled. Its incoming/outgoing asserted
            // links are committed by Consolidator; Reconcile must not invent entity state.
            if entity.source_memory_ids.is_empty() {
                continue;
            }
            let mut counted = ReconcileOutcome::default();
            match self
                .reconcile_entity(&path, proposals, &mut counted, replay_rejection.as_deref())
                .await
            {
                Ok(outcome) => {
                    *proposals = outcome.proposals;
                    self.validate_staged_projection(proposals).await?;
                    progress
                        .commit_reconciliation(
                            &path,
                            &outcome.promotions,
                            &outcome.retractions,
                            &counted,
                            proposals,
                            &outcome.commit,
                        )
                        .await?;
                    if path == SELF_ENTITY_PATH
                        && let Some(attributes) = outcome.profile_attributes.as_ref()
                    {
                        self.write_through_user_profile(attributes).await;
                    }
                    self.resolve_after_reconcile(
                        ReconciliationEffects {
                            path: &path,
                            promotions: &outcome.promotions,
                            retractions: &outcome.retractions,
                        },
                        proposals,
                        progress,
                        resolver,
                        ReconcileResolutionState {
                            resolve: resolve_state,
                            ontology_stats,
                            worklist: &mut worklist,
                        },
                    )
                    .await?;
                    for p in outcome.reconcile_dirty {
                        // Re-enqueue (back-edges allowed); the worklist only dedups
                        // simultaneous entries, so an entity can be reconciled again.
                        if worklist.push(p.clone()) {
                            // …and it is owed again, so the record must stop claiming it
                            // is finished - otherwise the skip above would suppress the
                            // revisit and strand the fact the union just handed it.
                            progress.reopen_reconciliation(&p);
                        }
                    }
                }
                Err(error) => {
                    // Keep this entity Pending and the pass before authoring. Accepted
                    // entities are already banked and need not be regenerated on retry.
                    warn!(%error, path = %path, "pkm reconcile: entity incomplete");
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn resolve_after_reconcile(
        &self,
        effects: ReconciliationEffects<'_>,
        proposals: &mut ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        resolver: &Consolidator,
        state: ReconcileResolutionState<'_>,
    ) -> Result<(), AppError> {
        let mut affected = std::collections::BTreeSet::from([effects.path.to_string()]);
        affected.extend(
            effects
                .promotions
                .iter()
                .map(|promotion| promotion.target.clone()),
        );
        affected.extend(effects.retractions.iter().map(|(_, target)| target.clone()));
        for candidate in affected {
            let candidate = progress.canonical_path(&candidate);
            let winner = resolver
                .resolve_identity(
                    &candidate,
                    proposals,
                    progress,
                    &mut *state.resolve,
                    &mut *state.ontology_stats,
                    true,
                )
                .await?;
            if let Some(winner) = winner {
                state.worklist.push(winner);
            }
        }
        Ok(())
    }

    /// No reconciled patch reaches the durable checkpoint unless the complete staged
    /// A-box and T-box remain valid together.
    async fn validate_staged_projection(&self, proposals: &ProposalSet) -> Result<(), AppError> {
        let validation = validate_proposal_projection(&self.ctx, &self.ontology, proposals).await?;
        if validation.is_valid() {
            return Ok(());
        }
        let details = projection_rejection_details(&validation);
        Err(AppError::Internal(format!(
            "reconcile invariant: staged A-box/T-box projection is invalid: {details}"
        )))
    }

    async fn reconcile_entity(
        &self,
        path: &str,
        proposals: &ProposalSet,
        stats: &mut ReconcileOutcome,
        initial_rejection: Option<&str>,
    ) -> Result<EntityOutcome, AppError> {
        let mut prompts = ReconcilePrompts::default();
        let llm = &self.ctx.llm;
        prompts.defaults = llm.prompt_defaults();
        prompts.system = llm.prompt_source("pkm/reconcile/system.md", &prompts.system)?;
        prompts.input = llm.prompt_source("pkm/reconcile/input.md", &prompts.input)?;
        prompts.reject = llm.prompt_source("pkm/reconcile/reject.md", &prompts.reject)?;
        prompts.promote = llm.prompt_source("pkm/reconcile/promote.md", &prompts.promote)?;
        let graph = self.ctx.view.portable_snapshot(&self.ctx.repo).await?;
        let mut memories = graph.memories_for_entity(path);
        if let Some(entity) = proposals.input_entity(path) {
            for memory in graph.memories_by_ids(&entity.source_memory_ids) {
                if !memories.iter().any(|held| held.id == memory.id) {
                    memories.push(memory);
                }
            }
        }
        let terminal = frona_pkm::model::terminal_task_plan_ids(&memories);
        let has_current = memories.iter().any(|memory| {
            memory.relations.is_empty()
                && memory.disposition == frona_pkm::model::Disposition::None
                && !terminal.contains(&memory.id)
        });
        let phase = frona_pkm::reconcile::Reconcile::new(llm.harness())
            .prompts(prompts)
            .submission_attempts(self.max_submissions);
        let mut output = phase
            .run_with(
                ReconcileInput {
                    path: path.to_string(),
                    graph,
                    ontology: self.ontology.portable(&self.ctx.scope.user_id).await?,
                    proposals: proposals.clone(),
                    initial_rejection: initial_rejection.map(str::to_owned),
                },
                |run| llm.configure(run),
            )
            .await?;
        stats.supersessions_recorded +=
            output.changes.memory_relations.len() + output.changes.outdated_memories.len();
        if output.entity_update.is_some() && has_current {
            stats.entities_reconciled += 1;
        }
        if let Some(mv) = output.move_entity.take()
            && self.ctx.view.entity_by_path(&mv.to).await?.is_none()
        {
            match self.ctx.rename_page_everywhere(&mv.from, &mv.to).await {
                Ok(()) => stats.moves_applied += 1,
                Err(error) => warn!(%error, "pkm reconcile: rename failed"),
            }
        }
        EntityOutcome::from_phase(path, output)
    }

    /// Project the self-entity's account-backed attributes onto the `User` record -
    /// a strict `{name, timezone}` allowlist. `timezone` is written only if it
    /// parses as a valid IANA zone (else skipped, so a bad value never corrupts
    /// scheduling). Everything else stays entity-only.
    async fn write_through_user_profile(&self, attributes: &serde_json::Value) {
        let Some(map) = attributes.as_object() else {
            return;
        };
        let name = map
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let timezone = map
            .get("timezone")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.parse::<chrono_tz::Tz>().is_ok());
        if name.is_none() && timezone.is_none() {
            return;
        }
        let user = match self.users.find_by_id(&self.ctx.scope.user_id).await {
            Ok(Some(u)) => u,
            Ok(None) => return,
            Err(e) => {
                warn!(error = %e, "pkm reconcile: user-profile write-through lookup failed");
                return;
            }
        };
        let mut updated = user.clone();
        let mut changed = false;
        if let Some(n) = name
            && updated.name != n
        {
            updated.name = n.to_string();
            changed = true;
        }
        if let Some(tz) = timezone
            && updated.timezone.as_deref() != Some(tz)
        {
            updated.timezone = Some(tz.to_string());
            changed = true;
        }
        if changed && let Err(e) = self.users.update(&updated).await {
            warn!(error = %e, "pkm reconcile: user-profile write-through update failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EntityOutcome;
    use frona_pkm::{
        classify::ProposalSet,
        model::{ConsolidationEntityLifecycle, EntityCategory, KnowledgeConsolidationEntity},
        reconcile::{ReconcileOutput, ReconciledEntityUpdate},
    };

    #[test]
    fn newly_queued_entity_is_carried_from_phase_to_durable_commit() {
        let path = "people/newly-dirtied";
        let before = ProposalSet::default();
        assert!(before.input_entity(path).is_none());
        let mut row = KnowledgeConsolidationEntity::pending(
            "run",
            "user",
            path,
            EntityCategory::Concept,
            Vec::new(),
            ["shared-memory".to_string()].into(),
        );
        row.name = "Casey".into();
        row.description = "Updated from a newly attached memory".into();
        row.attributes = serde_json::json!({"schema:jobTitle": "Engineer"});
        row.rederive_search();
        let mut accepted = before;
        accepted.stage_input_entity(row.clone());
        let output = ReconcileOutput {
            proposals: accepted,
            entity_update: Some(ReconciledEntityUpdate {
                name: row.name.clone(),
                description: row.description.clone(),
                attributes: row.attributes.clone(),
                attribute_sources: Vec::new(),
                declarations: Vec::new(),
            }),
            ..Default::default()
        };
        let outcome = EntityOutcome::from_phase(path, output).unwrap();
        let persisted = outcome
            .commit
            .entity
            .expect("new worklist entities must be committed");
        assert_eq!(persisted.path, path);
        assert_eq!(
            persisted.lifecycle,
            crate::memory::pkm::model::ConsolidationEntityLifecycle::Active
        );
        assert_eq!(
            outcome.proposals.input_entity(path).unwrap().lifecycle,
            ConsolidationEntityLifecycle::Active
        );
        assert_eq!(persisted.name, "Casey");
        assert_eq!(persisted.attributes["schema:jobTitle"], "Engineer");
        assert_eq!(persisted.source_memory_ids, ["shared-memory"]);
        assert!(outcome.proposals.input_entity(path).is_some());
    }
}
