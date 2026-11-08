use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::AtomicBool},
};

use tracing::warn;

use crate::{core::error::AppError, tool::registry::ToolFilter};
use crate::{
    db::repo::pkm::{
        ReconcileCommit, ReconcileEntityLinkSourceWrite, ReconcileMemoryRelationWrite,
        ReconcileOutdatedWrite,
    },
    memory::pkm::{
        consolidation::{
            ConsolidationProgress, Consolidator, PromptIds, PromptSpec, ReconcileOutcome,
            ReconcilePromotion, ResolveOutcome, ResolveWorkState,
            classify::ProposalSet,
            comparison_key, projection_rejection_details,
            reconcile::{
                EntityOutcome, EntityVerdict, Reconcile, ReconciledEntityUpdate,
                inspection::ReconciliationInspection,
                projection::{
                    curie_key, curie_key_attributes, drop_keys_held_as_relations,
                    render_attribute_lines, render_memory_lines,
                },
                validation::{
                    accepted_promotions, merge_explicit_retractions, replace_is_supported,
                    replacement_retractions,
                },
            },
            validate_proposal_projection,
        },
        model::{
            AttributeSource, EntityCategory, KnowledgeEntityLink, KnowledgeMemory, RelationType,
            SELF_ENTITY_PATH, terminal_task_plan_ids,
        },
        ontology::TermKind,
        storage::normalize_path,
    },
};
use frona_harness::{FailureKind, RunOutcome};

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

pub(super) fn partition_reconcile_memories(
    all_memories: &[KnowledgeMemory],
) -> (Vec<KnowledgeMemory>, Vec<KnowledgeMemory>) {
    let terminal_task_plans = terminal_task_plan_ids(all_memories);
    let current = all_memories
        .iter()
        .filter(|memory| {
            memory.relations.is_empty()
                && memory.disposition == crate::memory::pkm::model::Disposition::None
                && !terminal_task_plans.contains(&memory.id)
        })
        .cloned()
        .collect();
    let historical = all_memories
        .iter()
        .filter(|memory| {
            !memory.relations.is_empty()
                || memory.disposition != crate::memory::pkm::model::Disposition::None
                || terminal_task_plans.contains(&memory.id)
        })
        .cloned()
        .collect();
    (current, historical)
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
                        entity.attribute_sources,
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
                Ok(mut outcome) => {
                    if let Some(update) = outcome.page_update.as_ref() {
                        proposals.apply_reconciled_entity(
                            &path,
                            update.name.clone(),
                            update.description.clone(),
                            update.attributes.clone(),
                            update.attribute_sources.clone(),
                        );
                        proposals.add_reconcile_attributes(
                            &path,
                            &update.attributes,
                            &update.declarations,
                            &self.prefixes,
                        );
                        if let Some(mut entity) = proposals.input_entity(&path) {
                            entity.lifecycle =
                                crate::memory::pkm::model::ConsolidationEntityLifecycle::Active;
                            entity.rederive_search();
                            entity.validate()?;
                            outcome.commit.entity = Some(entity);
                        }
                    }
                    proposals.add_reconcile_promotions(
                        &path,
                        &outcome.promotions,
                        &outcome.retractions,
                        &self.prefixes,
                    );
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

    /// Compile one Reconcile submission into the exact graph snapshot it would stage.
    /// This is deliberately side-effect free: the caller validates the returned snapshot
    /// before memory relations, consolidation entities, or checkpoints are changed.
    pub(super) async fn reconcile_trial(
        &self,
        path: &str,
        entity: &crate::memory::pkm::model::KnowledgeConsolidationEntity,
        links: &[KnowledgeEntityLink],
        memories: &[KnowledgeMemory],
        verdict: &EntityVerdict,
        proposals: &ProposalSet,
    ) -> Result<ProposalSet, AppError> {
        let mut trial = proposals.clone();
        let valid: HashSet<&str> = memories.iter().map(|memory| memory.id.as_str()).collect();
        let retired_any = verdict
            .outdated
            .iter()
            .any(|item| valid.contains(item.memory.trim()))
            || verdict.relations.iter().any(|related| {
                valid.contains(related.memory.trim())
                    && related
                        .links
                        .iter()
                        .any(|link| !link.to.trim().is_empty() && valid.contains(link.to.trim()))
            });
        if !retired_any {
            let mut linked_names = HashSet::new();
            for link in links {
                let draft = proposals.entity_draft();
                let target = self
                    .ctx
                    .view
                    .entity_by_path_with(&draft, &link.to_entity_path)
                    .await?;
                if let Some(target) = target {
                    linked_names.insert(comparison_key(&target.name));
                    linked_names.extend(target.aliases.iter().map(|alias| comparison_key(alias)));
                }
            }
            let attributes = drop_keys_held_as_relations(
                curie_key_attributes(&verdict.attributes, &self.prefixes),
                links,
                &linked_names,
                &self.prefixes,
            );
            let attribute_sources = verdict
                .attribute_sources
                .iter()
                .map(|source| AttributeSource {
                    property: curie_key(&source.property, &self.prefixes),
                    value: source.value.clone(),
                    source_memory_ids: source.source_memory_ids.clone(),
                })
                .collect::<Vec<_>>();
            let description = if verdict.description.trim().is_empty() {
                entity.description.clone()
            } else {
                verdict.description.clone()
            };
            trial.apply_reconciled_entity(
                path,
                verdict.name.trim().to_string(),
                description,
                attributes.clone(),
                attribute_sources,
            );
            trial.add_reconcile_attributes(
                path,
                &attributes,
                &verdict.declarations,
                &self.prefixes,
            );
        }
        let promotions = accepted_promotions(
            verdict,
            &self.ctx,
            proposals,
            path,
            &self.prefixes,
            memories,
            proposals.promotions_for(path),
        )
        .await?;
        let retractions = replacement_retractions(
            verdict,
            &promotions,
            links,
            memories,
            &self.ctx,
            proposals,
            path,
        )
        .await?;
        let retractions = merge_explicit_retractions(retractions, verdict);
        trial.add_reconcile_promotions(path, &promotions, &retractions, &self.prefixes);
        Ok(trial)
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
        let mut all_memories = self
            .ctx
            .repo
            .memories_for_entity(&self.ctx.scope.user_id, path)
            .await?;
        let draft = proposals.entity_draft();
        let Some(entity) = self.ctx.view.entity_by_path_with(&draft, path).await? else {
            return Ok(EntityOutcome::default());
        };
        for memory in self
            .ctx
            .repo
            .memories_by_ids(&self.ctx.scope.user_id, &entity.source_memory_ids)
            .await?
        {
            if !all_memories.iter().any(|held| held.id == memory.id) {
                all_memories.push(memory);
            }
        }
        // Current memories are mutable verdict subjects. Historical memories are rendered
        // only so an Agent retrieval echo can be recognized without resurrecting them.
        // A terminal task event also makes the same task's planned episode historical;
        // the stable task identity comes from structured lifecycle evidence.
        let (memories, historical) = partition_reconcile_memories(&all_memories);
        if memories.is_empty() {
            return Ok(EntityOutcome {
                page_update: Some(ReconciledEntityUpdate {
                    name: String::new(),
                    description: "(no memories yet)".into(),
                    attributes: serde_json::json!({}),
                    attribute_sources: Vec::new(),
                    declarations: Vec::new(),
                }),
                ..EntityOutcome::default()
            });
        }

        let prompt_ids = PromptIds::new("m", all_memories.iter().map(|memory| memory.id.clone()));
        let mut memory_entities = HashMap::<String, Vec<String>>::new();
        for memory in &all_memories {
            let mut entities = self
                .ctx
                .repo
                .memory_entity_paths(&self.ctx.scope.user_id, &memory.id)
                .await?;
            entities.extend(proposals.memory_paths(&memory.id));
            entities.sort();
            entities.dedup();
            memory_entities.insert(prompt_ids.local(&memory.id).to_string(), entities);
        }
        let prompt_memories = memories
            .iter()
            .cloned()
            .map(|mut memory| {
                memory.id = prompt_ids.local(&memory.id).to_string();
                memory
            })
            .collect::<Vec<_>>();
        let prompt_historical = historical
            .iter()
            .cloned()
            .map(|mut memory| {
                memory.id = prompt_ids.local(&memory.id).to_string();
                memory
            })
            .collect::<Vec<_>>();
        let memory_lines = format!(
            "Current memories (verdict subjects):\n{}\nHistorical comparison memories (read-only targets):\n{}",
            render_memory_lines(&prompt_memories, &memory_entities),
            if prompt_historical.is_empty() {
                "(none)\n".to_string()
            } else {
                render_memory_lines(&prompt_historical, &memory_entities)
            },
        );
        // The model reads CURIEs; the database holds IRIs.
        let kinds = self.prefixes.display_joined(&entity.kinds);
        // What the entity already holds. Without these the stage is *stateless*: it derives
        // the whole attribute map from the memories each pass, blind to what is stored, so
        // nothing keeps a key stable across passes ("employer" one pass, "works at" the
        // next, two properties for one fact) and nothing stops it re-minting a literal for
        // something the Classify has already promoted to a relation.
        let attribute_lines = render_attribute_lines(&entity.attributes);
        let links = proposals.project_links(
            &self.ctx.scope.user_id,
            path,
            self.ctx
                .repo
                .links_from_entity(&self.ctx.scope.user_id, path)
                .await
                .unwrap_or_default(),
        );
        let relation_lines: String = links
            .iter()
            .map(|l| format!("- {} → {}\n", l.relation, l.to_entity_path))
            .collect();
        let vars = &[
            ("path", path),
            ("kind", &kinds),
            ("name", &entity.name),
            ("description", &entity.description),
            ("memories", &memory_lines),
            (
                "attributes",
                if attribute_lines.is_empty() {
                    "(none)\n"
                } else {
                    &attribute_lines
                },
            ),
            (
                "relations",
                if relation_lines.is_empty() {
                    "(none)\n"
                } else {
                    &relation_lines
                },
            ),
        ];

        // Reconcile has no tools or Chat. Only explicitly accepted inspection
        // results may reach its application transaction and completion checkpoint.
        let suffix = initial_rejection.map(|rejection| format!(
            "\n\nThe checkpointed proposal is no longer valid against the reconstructed graph. Revise it before it can be staged:\n{rejection}"
        )).unwrap_or_default();
        let harness = self
            .ctx
            .llm
            .sdk(&self.ctx.scope.agent_id, &[ToolFilter::AllowList(&[])], &[])
            .await?;
        let run = harness
            .run_with_input_suffix(PromptSpec::RECONCILE, vars, &suffix)?
            .structured::<EntityVerdict>()
            .submission_attempts(self.max_submissions);

        let mut prompt_attribute_sources = entity.attribute_sources.clone();
        for source in &mut prompt_attribute_sources {
            for id in &mut source.source_memory_ids {
                *id = prompt_ids.local(id).to_string();
            }
        }
        let mut prompt_links = links.clone();
        for link in &mut prompt_links {
            for id in &mut link.source_memory_ids {
                *id = prompt_ids.local(id).to_string();
            }
        }
        let catalogue = self.ontology.catalog(&self.ctx.scope.user_id).await.ok();
        let mut known_object_properties: HashSet<String> = catalogue
            .as_ref()
            .map(|catalogue| catalogue.object_properties.iter().cloned().collect())
            .unwrap_or_default();
        let mut known_data_properties: HashSet<String> = catalogue
            .as_ref()
            .map(|catalogue| catalogue.data_properties.iter().cloned().collect())
            .unwrap_or_default();
        for edit in &proposals.proposed_edits {
            match edit {
                crate::memory::pkm::ontology::SchemaEdit::DeclareObjectProperty { property } => {
                    known_object_properties.insert(property.clone());
                }
                crate::memory::pkm::ontology::SchemaEdit::DeclareDataProperty { property } => {
                    known_data_properties.insert(property.clone());
                }
                _ => {}
            }
        }
        let draft = proposals.entity_draft();
        let suggestion_entities = self.ctx.view.snapshot_with(&draft).await?.into_entities();
        let inspection = Arc::new(ReconciliationInspection {
            reconciler: self.clone(),
            path: path.to_string(),
            current: prompt_memories,
            scopes: memory_entities,
            attribute_sources: prompt_attribute_sources,
            prompt_links,
            known_object_properties,
            known_data_properties,
            suggestion_entities,
            suggestions_sent: AtomicBool::new(false),
            proposals: proposals.clone(),
            page: entity.clone(),
            links: links.clone(),
            memories: memories.clone(),
            ids: prompt_ids.clone(),
        });
        let refined = run
            .inspect(move |candidate| {
                let inspection = inspection.clone();
                async move { inspection.inspect(candidate).await }
            })
            .await;
        let mut parsed = match refined {
            Ok(accepted) => accepted,
            Err(incomplete) => {
                warn!(reason = %incomplete.reason, outcome = ?incomplete.report.outcome,
                    path = %path, "pkm reconcile: refinement incomplete");
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
                return Err(AppError::Internal(format!(
                    "reconciliation incomplete: {}",
                    incomplete.reason
                )));
            }
        };
        parsed.expand_prompt_ids(&prompt_ids)?;

        let valid: HashSet<&str> = memories.iter().map(|m| m.id.as_str()).collect();
        let valid_targets: HashSet<&str> = all_memories.iter().map(|m| m.id.as_str()).collect();
        let mut reconcile_dirty: HashSet<String> = HashSet::new();
        let mut retired_any = false;
        let mut commit = ReconcileCommit::default();

        // Binary relations are global, so they are safe only after the refine guard has
        // established identical scope for duplicate/absorbed, or a shared reconciliation
        // entity plus any required typed assertion transition for replace.
        for related in &parsed.relations {
            let sub = related.memory.trim();
            if !valid.contains(sub) {
                continue;
            }
            for link in &related.links {
                let to = link.to.trim();
                if to.is_empty() || to == sub || !valid_targets.contains(to) {
                    continue;
                }
                // The guard, applied where it counts. A `replace` the model never
                // justified is dropped rather than downgraded: `Duplicate`/`Absorbed` are
                // deleted outright by cleanup, so guessing wrong there would destroy a
                // fact. Dropping the link leaves both memories current - the entity briefly
                // shows two values and the next pass reconciles them again.
                if link.relation == RelationType::Replace
                    && !link.derived_from_property
                    && !replace_is_supported(link, sub, to, &memories)
                {
                    warn!(
                        entity = %path,
                        older = %sub,
                        newer = %to,
                        was = %link.was,
                        now = %link.now,
                        "pkm reconcile: unjustified replace dropped — both entries stay current"
                    );
                    continue;
                }
                commit.memory_relations.push(ReconcileMemoryRelationWrite {
                    subordinate_id: sub.to_string(),
                    relation: link.relation,
                    to_id: to.to_string(),
                    note: link.note.trim().to_string(),
                });
                stats.supersessions_recorded += 1;
                retired_any = true;
            }
        }

        for o in &parsed.outdated {
            let m = o.memory.trim();
            if !valid.contains(m) {
                continue;
            }
            commit.outdated_memories.push(ReconcileOutdatedWrite {
                memory_id: m.to_string(),
                reason: o.note.trim().to_string(),
            });
            stats.supersessions_recorded += 1;
            retired_any = true;
        }

        if retired_any {
            // The description and attributes were derived from a memory set we just
            // changed, so defer those until the clean rerun. Relation transitions are
            // different: they describe the same accepted state change and must not be
            // discarded, because the old memory will be historical on that rerun.
            let promotions = accepted_promotions(
                &parsed,
                &self.ctx,
                proposals,
                path,
                &self.prefixes,
                &memories,
                proposals.promotions_for(path),
            )
            .await?;
            let retractions = replacement_retractions(
                &parsed,
                &promotions,
                &links,
                &memories,
                &self.ctx,
                proposals,
                path,
            )
            .await?;
            let retractions = merge_explicit_retractions(retractions, &parsed);
            reconcile_dirty.insert(path.to_string());
            return Ok(EntityOutcome {
                reconcile_dirty,
                promotions,
                retractions,
                page_update: None,
                commit,
                profile_attributes: None,
            });
        }

        let description = if parsed.description.trim().is_empty() {
            entity.description.clone()
        } else {
            parsed.description.clone()
        };
        // The ontology layer is always on, so store attribute keys as CURIEs
        // (YAML-LD-liftable): a standard-looking key maps to its standard property,
        // the rest to a `frona:` term (used-but-undeclared - the Classify stage
        // aligns/declares it later). The account write-through below stays on the
        // ORIGINAL keys, so it is untouched.
        // The prompt asks for this; the filter is what guarantees it. A key already held
        // as a relation on this entity means the Classify stage decided that fact is an edge,
        // so it must not also be derived as a literal from the same memory.
        // Every name the entities this one links to go by - what an attribute value has to
        // match to be the same fact as an existing edge.
        let mut linked_names: HashSet<String> = HashSet::new();
        for l in &links {
            let draft = proposals.entity_draft();
            let target = self
                .ctx
                .view
                .entity_by_path_with(&draft, &l.to_entity_path)
                .await?;
            if let Some(target) = target {
                linked_names.insert(comparison_key(&target.name));
                linked_names.extend(target.aliases.iter().map(|a| comparison_key(a)));
            }
        }
        let attributes = drop_keys_held_as_relations(
            curie_key_attributes(&parsed.attributes, &self.prefixes),
            &links,
            &linked_names,
            &self.prefixes,
        );
        let attribute_sources = parsed
            .attribute_sources
            .iter()
            .map(|source| AttributeSource {
                property: curie_key(&source.property, &self.prefixes),
                value: source.value.clone(),
                source_memory_ids: source.source_memory_ids.clone(),
            })
            .collect::<Vec<_>>();
        let promotions = accepted_promotions(
            &parsed,
            &self.ctx,
            proposals,
            path,
            &self.prefixes,
            &memories,
            proposals.promotions_for(path),
        )
        .await?;
        let retractions = replacement_retractions(
            &parsed,
            &promotions,
            &links,
            &memories,
            &self.ctx,
            proposals,
            path,
        )
        .await?;
        let retractions = merge_explicit_retractions(retractions, &parsed);
        for relation in &parsed.entity_relations {
            let property = self
                .prefixes
                .repair_term(relation.property.trim(), TermKind::Property)
                .unwrap_or_else(|_| relation.property.trim().to_string());
            commit
                .entity_link_sources
                .push(ReconcileEntityLinkSourceWrite {
                    from_entity_path: path.to_string(),
                    to_entity_path: relation.target.trim().to_string(),
                    relation: property,
                    source_memory_ids: relation.source_memory_ids.clone(),
                });
        }
        stats.entities_reconciled += 1;

        if let Some(mv) = parsed.moves.into_iter().next()
            && let (Some(from), Some(to)) = (normalize_path(&mv.from), normalize_path(&mv.to))
            && from == path
            && to != from
            && self.ctx.view.entity_by_path(&to).await?.is_none()
        {
            match self.ctx.rename_page_everywhere(&from, &to).await {
                Ok(()) => stats.moves_applied += 1,
                Err(e) => warn!(error = %e, "pkm reconcile: rename failed"),
            }
        }
        Ok(EntityOutcome {
            reconcile_dirty,
            promotions,
            retractions,
            page_update: Some(ReconciledEntityUpdate {
                name: parsed.name.trim().to_string(),
                description,
                attributes,
                attribute_sources,
                declarations: parsed.declarations.clone(),
            }),
            commit,
            profile_attributes: (path == SELF_ENTITY_PATH).then(|| parsed.attributes.clone()),
        })
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
