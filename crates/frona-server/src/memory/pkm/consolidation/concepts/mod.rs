//! Coordinate classification, identity, reconciliation, and publication as one durable workflow.

mod assemble;
mod classify;
mod progress;
mod reconcile;
mod resolve;
mod validation;

use frona_pkm::projection::is_unbacked_shell;
use std::collections::{BTreeMap, HashSet, VecDeque};
use tracing::warn;

use crate::core::error::AppError;
use crate::memory::pkm::consolidation::driver::Consolidator;
use crate::memory::pkm::consolidation::{
    AssembleOutcome, ClassifyOutcome, ConsolidationStageState, ResolveOutcome,
};
use crate::memory::pkm::model::{
    ClassificationProgress, EntityCategory, KnowledgeConsolidationEntity,
};
use classify::ClassificationMode;
use frona_pkm::classify::ProposalSet;
use frona_pkm::model::merge_consolidation_attribute_values;
use progress::{ConsolidationProgress, WorkStage};
use resolve::ResolveWorkState;

#[derive(Debug, Clone, Copy)]
struct ClassificationJob {
    entity_index: usize,
    mode: ClassificationMode,
}

impl Consolidator {
    pub(crate) async fn execute(
        &self,
    ) -> Result<(ClassifyOutcome, ResolveOutcome, AssembleOutcome), AppError> {
        let mut classify_out = ClassifyOutcome::default();
        let mut resolve_out = ResolveOutcome::default();
        let mut assemble_out = AssembleOutcome::default();
        let classify = &mut classify_out;
        let resolve = &mut resolve_out;
        let assemble = &mut assemble_out;
        let progress = &mut ConsolidationProgress::open(&self.ctx).await?;
        let ontology_manager = self.ontology.clone();
        let user_id = self.ctx.scope.user_id.clone();

        // Build the pass's virtual entity set. Raw extractor output lives only in the
        // checkpoint; merging it here gives classify its virtual entity shape without
        // exposing free-text keys to the live A-box.
        let mut pages_by_path: BTreeMap<String, KnowledgeConsolidationEntity> = BTreeMap::new();
        for path in self
            .ctx
            .repo
            .entities_needing_reconciliation(&user_id)
            .await?
        {
            if let Some(p) = self.ctx.view.entity_by_path(&path).await?
                && p.category == EntityCategory::Concept
            {
                pages_by_path.insert(path, p);
            }
        }
        let pending_entities: Vec<_> = progress.concept_rows().cloned().collect();
        for candidate in &pending_entities {
            let staged_attributes = candidate.staged_attributes();
            if !pages_by_path.contains_key(&candidate.path)
                && let Some(entity) = self.ctx.view.entity_by_path(&candidate.path).await?
                && (!candidate.existing_only() || entity.entity_id.is_some())
            {
                pages_by_path.insert(candidate.path.clone(), entity);
            }
            if let Some(entity) = pages_by_path.get_mut(&candidate.path) {
                if matches!(
                    entity.progress.classification,
                    ClassificationProgress::Accepted { .. }
                ) {
                    continue;
                }
                if let (Some(held), Some(offered)) = (
                    entity.attributes.as_object_mut(),
                    staged_attributes.as_object(),
                ) {
                    for (key, value) in offered {
                        match held.get_mut(key) {
                            Some(existing) => merge_consolidation_attribute_values(existing, value),
                            None => {
                                held.insert(key.clone(), value.clone());
                            }
                        }
                    }
                }
                entity.aliases.extend(candidate.aliases.iter().cloned());
                for memory_id in &candidate.source_memory_ids {
                    if !entity.source_memory_ids.contains(memory_id) {
                        entity.source_memory_ids.push(memory_id.clone());
                    }
                }
                continue;
            }
            // A memory path alone is not authority to mint an entity. Relationship routing
            // preserves such paths as update-only placeholders so an existing entity can
            // receive the memory above; when the lookup finds nothing, keep the path on
            // the durable memory for repair but do not turn it into an entity candidate.
            if candidate.existing_only() {
                continue;
            }
            let name = if candidate.name.trim().is_empty() {
                candidate
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&candidate.path)
                    .to_string()
            } else {
                candidate.name.clone()
            };
            let mut entity = KnowledgeConsolidationEntity::pending(
                self.ctx.view.consolidation_id(),
                &user_id,
                &candidate.path,
                EntityCategory::Concept,
                candidate.contributions.clone(),
                candidate.source_memory_ids.iter().cloned().collect(),
            );
            entity.name = name;
            entity.description = candidate.description.clone();
            entity.aliases = candidate.aliases.clone();
            entity.attributes = staged_attributes;
            entity.rederive_search();
            pages_by_path.insert(candidate.path.clone(), entity);
        }
        let entities: Vec<_> = pages_by_path.into_values().collect();
        // A new extracted entity with no memory is only a name/description shell. Keep it
        // in the temporary entity view so a backed entity can discover it as an identity or
        // relation target, but do not classify, reconcile, author, or materialize it.
        // Existing live entities are never shells, even when an old row has no provenance.
        let shell_paths: HashSet<String> = entities
            .iter()
            .filter(|entity| is_unbacked_shell(&(*entity).clone().into()))
            .map(|entity| entity.path.clone())
            .collect();

        // Nothing is committed or stamped before Assemble accepts the complete proposal.
        // Entities this pass put into quarantine, from any of the three routes. They must
        // not be released by the reinstatement sweep at the end of the same pass.
        let mut quarantined: HashSet<String> = HashSet::new();
        let mut proposals = ProposalSet::default();
        for entity in &entities {
            proposals.stage_input_entity(entity.clone().into());
            if entity.entity_id.is_none() && !shell_paths.contains(&entity.path) {
                proposals.stage_entity(entity.clone().into());
            }
        }
        // Entities the Classify stage created from an attribute value naming an entity nothing had
        // an entity for. They arrive after `entities` was read, so they are carried separately -
        // resolve has to see them or a mint can duplicate an entity the search missed.
        let mut minted: Vec<String> = Vec::new();
        let mut classification_jobs: VecDeque<_> = entities
            .iter()
            .enumerate()
            .filter(|(_, entity)| !shell_paths.contains(&entity.path))
            .map(|(entity_index, _)| ClassificationJob {
                entity_index,
                mode: ClassificationMode::Normal,
            })
            .collect();
        let mut classification_failure: Option<AppError> = None;
        let mut referenced_shell_paths = HashSet::new();
        let mut shell_jobs_added = false;
        loop {
            let Some(job) = classification_jobs.pop_front() else {
                if shell_jobs_added {
                    break;
                }
                if let Some(error) = classification_failure.take() {
                    return Err(error);
                }
                // A shell becomes an A-box individual only when an accepted mapping
                // points at it. Add these jobs after every normal entity has produced
                // its edges, in the same stable entity order as before.
                referenced_shell_paths = proposals
                    .referenced_targets()
                    .intersection(&shell_paths)
                    .cloned()
                    .collect();
                classification_jobs.extend(
                    entities
                        .iter()
                        .enumerate()
                        .filter(|(_, entity)| referenced_shell_paths.contains(&entity.path))
                        .map(|(entity_index, _)| ClassificationJob {
                            entity_index,
                            mode: ClassificationMode::ReferencedShell,
                        }),
                );
                shell_jobs_added = true;
                continue;
            };
            let entity = &entities[job.entity_index];
            if let Err(error) = self
                .process_classification_job(
                    &ontology_manager,
                    entity,
                    job.mode,
                    &mut proposals,
                    progress,
                    &mut minted,
                )
                .await
            {
                if job.mode.defers_error() {
                    warn!(error = %error, entity = %entity.path, "pkm classify: classify failed");
                    classification_failure.get_or_insert(error);
                } else {
                    return Err(error);
                }
            }
        }

        let unresolved = progress
            .concept_rows()
            .filter(|candidate| {
                !candidate.existing_only()
                    && !candidate.source_memory_ids.is_empty()
                    && !proposals.by_path.contains_key(&candidate.path)
                    && !progress.is_resolved(&candidate.path)
            })
            .count();
        if unresolved != 0 {
            return Err(AppError::Internal(format!(
                "classify: {unresolved} extracted entity candidates remain unclassified"
            )));
        }
        if matches!(self.ctx.stage().await, ConsolidationStageState::Classify(_)) {
            progress.advance_to(WorkStage::Resolve).await?;
        }

        // Candidate type filtering reasons over `scope ⊕ delta ⊕ TEMP ⊕ proposed
        // types`, so it accounts for terms this pass has proposed but not committed.
        // Minted entities resolve alongside the mentions. They were created from a value the
        // search did not match to anything, which is exactly the case where the entity it
        // meant may already exist under a name the search missed - leaving them out would
        // make minting a duplicate-entity generator.
        // Resolve shells first, and only into memory-backed entities. Backed entities may then
        // resolve among themselves, but never into a shell: provenance always wins as the
        // canonical identity. Shell-to-shell merging has no durable knowledge to preserve.
        let mut backed_paths: HashSet<String> = self
            .ctx
            .view
            .list_entities()
            .await?
            .into_iter()
            .filter(|entity| {
                entity.category == EntityCategory::Concept && entity.entity_id.is_some()
            })
            .map(|entity| entity.path)
            .collect();
        backed_paths.extend(
            entities
                .iter()
                .filter(|entity| !shell_paths.contains(&entity.path))
                .map(|entity| entity.path.clone())
                .chain(minted.iter().cloned()),
        );
        let mut identities: Vec<String> = shell_paths.iter().cloned().collect();
        identities.sort();
        identities.extend(
            entities
                .iter()
                .filter(|entity| !shell_paths.contains(&entity.path))
                .map(|entity| entity.path.clone()),
        );
        identities.extend(minted.iter().cloned());
        let previously_merged = progress.resolved_into().keys().cloned().collect();
        let mut resolve_state =
            ResolveWorkState::new(backed_paths, shell_paths.clone(), previously_merged);
        resolve.resolve_sweeps += 1;
        for path in &identities {
            self.resolve_identity(
                path,
                &mut proposals,
                progress,
                &mut resolve_state,
                resolve,
                false,
            )
            .await?;
        }

        // An unmerged shell has served its only entity-level purpose. Forgetting it prevents
        // classification commit and reconciliation from materializing an unsupported entity.
        // Any sourced promotion that already names the path remains a graph edge, whose
        // object IRI exists in the A-box without requiring a knowledge_entity row.
        for path in &shell_paths {
            if !resolve_state.is_merged(path)
                && proposals
                    .input_entity(path)
                    .is_some_and(|entity| frona_pkm::projection::is_unbacked_shell(&entity))
                && !referenced_shell_paths.contains(path)
            {
                proposals.forget(path);
            }
        }

        classify.entities_minted += minted
            .iter()
            .filter(|p| !resolve_state.is_merged(p))
            .count();

        if matches!(self.ctx.stage().await, ConsolidationStageState::Resolve(_)) {
            progress.advance_to(WorkStage::Reconcile).await?;
        }

        // Reconcile after one complete Resolve sweep. Each accepted entity refresh runs an
        // incremental fingerprint check, and a resulting merge reopens only its winner;
        // fail-safe promotions remain in `proposals` until the single final commit.
        reconcile::Reconcile {
            ctx: self.ctx.clone(),
            ontology: ontology_manager.clone(),
            users: self.user_service.clone(),
            prefixes: self.prefixes.clone(),
            max_submissions: self.config.pkm_consolidation_max_submissions,
        }
        .run(&mut proposals, progress, self, &mut resolve_state, resolve)
        .await?;

        if matches!(
            self.ctx.stage().await,
            ConsolidationStageState::Reconcile(_)
        ) {
            progress.advance_to(WorkStage::Assemble).await?;
        }

        assemble.entities_typed += self
            .assemble(
                &ontology_manager,
                &entities,
                &proposals,
                assemble,
                &mut quarantined,
                progress,
            )
            .await?;

        Ok((classify_out, resolve_out, assemble_out))
    }
}
