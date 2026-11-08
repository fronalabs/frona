use std::collections::{BTreeMap, HashSet, VecDeque};
use tracing::warn;

use crate::core::error::AppError;
use crate::memory::pkm::consolidation::classify::ProposalSet;
use crate::memory::pkm::consolidation::context::ConsolidationContext;
use crate::memory::pkm::consolidation::driver::Consolidator;
use crate::memory::pkm::consolidation::progress::{ConsolidationProgress, WorkStage};
use crate::memory::pkm::consolidation::projection::{
    is_unbacked_shell, projection_rejection_details, validate_proposal_projection,
};
use crate::memory::pkm::consolidation::{
    AssembleOutcome, ClassifyOutcome, ConsolidationStageState, PromptSpec, ResolveOutcome,
    prompt_evidence,
};
use crate::memory::pkm::consolidation::{reconcile, resolve};
use crate::memory::pkm::model::{
    ClassificationProgress, EntityCategory, KnowledgeConsolidationEntity, SELF_ENTITY_PATH,
    merge_consolidation_attribute_values,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassificationMode {
    Normal,
    ReferencedShell,
}

impl ClassificationMode {
    const fn replay_stage(self) -> &'static str {
        match self {
            Self::Normal => "classify-replay",
            Self::ReferencedShell => "classify-referenced-shell-replay",
        }
    }

    const fn discard_reason(self) -> &'static str {
        match self {
            Self::Normal => "classification budget exhausted without a valid projection",
            Self::ReferencedShell => "referenced identity could not be classified",
        }
    }

    const fn stages_entity(self) -> bool {
        matches!(self, Self::ReferencedShell)
    }

    const fn defers_error(self) -> bool {
        matches!(self, Self::Normal)
    }
}

#[derive(Debug, Clone, Copy)]
struct ClassificationJob {
    entity_index: usize,
    mode: ClassificationMode,
}

pub(super) struct ResolveWorkState {
    backed_paths: HashSet<String>,
    shell_paths: HashSet<String>,
    merged: HashSet<String>,
    incremental_started: bool,
}

impl ResolveWorkState {
    fn new(
        backed_paths: HashSet<String>,
        shell_paths: HashSet<String>,
        merged: HashSet<String>,
    ) -> Self {
        Self {
            backed_paths,
            shell_paths,
            merged,
            incremental_started: false,
        }
    }

    pub(super) fn is_merged(&self, path: &str) -> bool {
        self.merged.contains(path)
    }
}

/// The pushback for any CURIEs in a submission that cannot be written to the schema, or
/// `None` when every term is usable.
///
/// Both conversations that mint terms run this before their own validation, because an
/// unwritable term is not a wrong answer to be reasoned about - it is a string that, once
/// committed, makes the user's delta unparseable and every later schema call on it fail.
/// The reasoner cannot catch it: `expand` is total, so `frona:firmware download` becomes an
/// IRI without complaint and only the *next* read discovers there is no way back.
///
/// `stage` selects whose `bad_term.md` explains what to re-send.
///
/// `px` comes from [`OntologyManager::prefixes`] - the bindings the catalogue actually
/// holds, never a fresh `PrefixMap::standard()`. Which prefixes are bound is exactly what
/// decides whether a term is legal, so reconstructing that set here would reject a term as
/// unusable the moment the catalogue bound anything the reconstruction did not know about.
/// Refusing a term the model got right is worse than the bug this guards against.
///
/// [`OntologyManager::prefixes`]: crate::memory::pkm::ontology::OntologyManager::prefixes
pub(super) fn bad_term_feedback(
    ctx: &ConsolidationContext,
    stage: PromptSpec,
    px: &crate::memory::pkm::ontology::PrefixMap,
    terms: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<Option<String>, AppError> {
    let bad: Vec<String> = terms
        .into_iter()
        .filter_map(|t| px.validate_term(t.as_ref()).err())
        .map(|e| format!("- {e}"))
        .collect();
    if bad.is_empty() {
        return Ok(None);
    }
    // Worth a log line of its own: the prompts ask for a legal term, so reaching here
    // means the instruction did not take, and the count over a run is the only signal for
    // whether that is getting better or worse.
    warn!(count = bad.len(), stage = stage.dir, terms = %bad.join(" "), "ontology: unusable terms");
    ctx.llm
        .bad_term(stage, &[("terms", bad.join("\n").as_str())])
        .map(Some)
}

impl Consolidator {
    pub(super) async fn tool_overlay(
        &self,
        ontology_manager: &crate::memory::pkm::ontology::OntologyManager,
        proposals: &ProposalSet,
    ) -> Result<
        std::sync::Arc<crate::memory::pkm::consolidation::tools::ontology::OntologyToolOverlay>,
        AppError,
    > {
        let draft = proposals.entity_draft();
        let entities = self.ctx.view.snapshot_with(&draft).await?.into_entities();
        let abox = self
            .projected_abox(ontology_manager, proposals, None)
            .await?;
        Ok(std::sync::Arc::new(
            crate::memory::pkm::consolidation::tools::ontology::OntologyToolOverlay {
                entities,
                proposed_edits: proposals.proposed_edits.clone(),
                abox,
                diagnostics: Default::default(),
                prefixes: self.prefixes.clone(),
                tool_budget: usize::MAX,
                tool_calls: Default::default(),
            },
        ))
    }

    pub(super) async fn resolve_identity(
        &self,
        path: &str,
        proposals: &mut ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        state: &mut ResolveWorkState,
        stats: &mut ResolveOutcome,
        after_first_sweep: bool,
    ) -> Result<Option<String>, AppError> {
        if path == SELF_ENTITY_PATH || state.merged.contains(path) {
            return Ok(None);
        }
        if progress.canonical_path(path) != path {
            return Ok(None);
        }
        if !after_first_sweep && progress.is_resolved(path) {
            return Ok(None);
        }
        let draft = proposals.entity_draft();
        let Some(stored) = self.ctx.view.entity_by_path_with(&draft, path).await? else {
            return Ok(None);
        };
        let staged_type_delta = self
            .ontology
            .plan_schema(&self.ctx.scope.user_id, &proposals.proposed_edits)
            .await?
            .triples;
        let kinds = self.ontology.normalize_types(
            &proposals.kinds_for(path, &stored.kinds),
            &staged_type_delta,
        );
        if !state.shell_paths.contains(path) && kinds.iter().all(|kind| kind.trim().is_empty()) {
            return Ok(None);
        }
        let mut typed = proposals.project_entity(stored);
        typed.kinds = kinds;
        typed.rederive_search();
        let has_keys = proposals.provisional_has_keys();
        let inverse_functional = proposals.provisional_inverse_functional_properties();
        let identity_fingerprint = resolve::resolution_identity_fingerprint(
            &typed,
            &has_keys,
            &inverse_functional,
            &self.prefixes,
        );
        if after_first_sweep
            && progress.resolution_fingerprint(path) == Some(identity_fingerprint.as_str())
        {
            stats.resolve_fingerprint_skips += 1;
            return Ok(None);
        }
        if after_first_sweep {
            stats.resolve_identity_state_changes += 1;
        }
        let projected_abox = self.projected_abox(&self.ontology, proposals, None).await?;
        let pass = self
            .ontology
            .reason_user_with_proposed(
                &self.ctx.scope.user_id,
                &proposals.proposed_edits,
                &projected_abox,
            )
            .await?;
        let mut candidates = self
            .resolution_candidates(
                &self.ontology,
                &pass.reasoned.store,
                &typed,
                proposals,
                &state.backed_paths,
                &staged_type_delta,
            )
            .await?;
        if after_first_sweep && !state.incremental_started {
            state.incremental_started = true;
            stats.resolve_sweeps += 1;
        }
        stats.resolve_candidate_evaluations += 1;
        if after_first_sweep {
            stats.resolve_candidate_evaluations_after_first_sweep += 1;
        }
        let mut identifying_matches: BTreeMap<String, Vec<resolve::IdentityMatch>> = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.entity.path.clone(),
                    resolve::identity_matches(
                        &typed,
                        &candidate.entity,
                        &has_keys,
                        &inverse_functional,
                        &self.prefixes,
                    ),
                )
            })
            .collect();
        if after_first_sweep {
            let mut weaker_or_removed = Vec::new();
            candidates.retain(|candidate| {
                let matches = identifying_matches
                    .get(&candidate.entity.path)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let key = resolve::resolution_pair_key(&typed.path, &candidate.entity.path);
                let fingerprint =
                    resolve::resolution_pair_fingerprint(&typed, &candidate.entity, matches);
                let old = progress.resolution_pair_fingerprint(&key);
                let should_judge = if matches.is_empty() && old.is_none() {
                    false
                } else {
                    resolve::pair_change_requires_judgment(old, &fingerprint)
                };
                if !should_judge && old != Some(fingerprint.as_str()) {
                    weaker_or_removed.push((key, fingerprint));
                    stats.resolve_identity_pair_weakenings += 1;
                }
                if should_judge {
                    stats.resolve_identity_pair_changes += 1;
                }
                if !should_judge {
                    stats.resolve_fingerprint_skips += 1;
                }
                should_judge
            });
            if !weaker_or_removed.is_empty() {
                progress.bank_resolution_pairs(weaker_or_removed).await?;
            }
            identifying_matches.retain(|path, _| {
                candidates
                    .iter()
                    .any(|candidate| candidate.entity.path == *path)
            });
            if candidates.is_empty() {
                progress
                    .commit_resolved_distinct(
                        path,
                        identity_fingerprint,
                        serde_json::json!({"reason": "no_changed_identity_candidates"}),
                        proposals,
                    )
                    .await?;
                return Ok(None);
            }
        } else {
            candidates.sort_by_key(|candidate| {
                identifying_matches
                    .get(&candidate.entity.path)
                    .is_none_or(Vec::is_empty)
            });
        }
        candidates.truncate(crate::memory::pkm::consolidation::candidates::RESOLUTION_PROMPT_LIMIT);
        identifying_matches.retain(|path, _| {
            candidates
                .iter()
                .any(|candidate| candidate.entity.path == *path)
        });
        // Resolve validates the complete post-merge graph inside its model loop. Stage
        // the offered live candidates in the in-memory projection so that trial merge
        // sees their full attributes, links, classes, and provenance rather than only
        // the lightweight search hit shown to the model.
        for candidate in &candidates {
            if proposals.input_entity(&candidate.entity.path).is_none()
                && let Some(entity) = self.ctx.view.entity_by_path(&candidate.entity.path).await?
            {
                proposals.stage_input_entity(entity);
            }
        }
        let mut candidate_identity_evidence = BTreeMap::new();
        for candidate in &candidates {
            let evidence = match proposals.input_entity(&candidate.entity.path) {
                Some(entity) => prompt_evidence(&entity.identity_evidence),
                None => self
                    .ctx
                    .view
                    .entity_by_path(&candidate.entity.path)
                    .await?
                    .map(|entity| prompt_evidence(&entity.identity_evidence))
                    .unwrap_or_else(|| "(none)".into()),
            };
            candidate_identity_evidence.insert(candidate.entity.path.clone(), evidence);
        }
        let decision_context = resolve::ResolutionDecisionContext::new(
            &typed,
            &candidates,
            &candidate_identity_evidence,
            &identifying_matches,
            &self.prefixes,
        );
        stats.resolve_decision_attempts += 1;
        if after_first_sweep {
            stats.resolve_reconsiderations += 1;
        }
        let resolved = if candidates.is_empty() {
            None
        } else {
            stats.resolve_conversations += 1;
            if after_first_sweep {
                stats.resolve_reconsideration_conversations += 1;
            }
            match self
                .adjudicate_identity(
                    &self.ontology,
                    &typed,
                    &candidates,
                    &decision_context,
                    proposals,
                )
                .await
            {
                Ok(conversation) => {
                    stats.resolve_evidence_corrections += conversation.corrections;
                    Some(conversation.decision)
                }
                Err(error) => {
                    warn!(%error, entity = %path, "pkm resolve: resolve conversation failed");
                    // An interrupted investigation must remain retryable. Advancing
                    // this stage would hide its pending identity decision.
                    return Err(error);
                }
            }
        };
        let Some(resolution) = resolved else {
            progress.remember_resolution_pairs(decision_context.pair_fingerprints);
            progress
                .commit_resolved_distinct(
                    path,
                    identity_fingerprint,
                    serde_json::json!({"reason": "no_identity_candidates"}),
                    proposals,
                )
                .await?;
            return Ok(None);
        };

        progress.remember_resolution_pairs(decision_context.pair_fingerprints);

        let (canonical, same_as, evidence) = match resolution {
            resolve::IdentityResolution::Distinct { evidence } => {
                let count = evidence
                    .get("distinct_because")
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len);
                stats.resolve_distinct_with_evidence += count;
                progress
                    .commit_resolved_distinct(path, identity_fingerprint, evidence, proposals)
                    .await?;
                return Ok(None);
            }
            resolve::IdentityResolution::Unresolved {
                diagnostic,
                pair_count,
            } => {
                stats.resolve_unresolved_pairs += pair_count;
                progress
                    .commit_resolution_unresolved(path, identity_fingerprint, diagnostic, proposals)
                    .await?;
                return Ok(None);
            }
            resolve::IdentityResolution::Merge {
                canonical,
                same_as,
                evidence,
            } => {
                let count = evidence
                    .get("merge_because")
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len);
                stats.resolve_merges_with_evidence += count;
                (canonical, same_as, evidence)
            }
        };
        progress.remember_resolution_evidence(path, evidence);

        let into = progress.canonical_path(&canonical);
        if proposals.input_entity(&into).is_none() {
            let canonical = self.ctx.view.entity_by_path(&into).await?.ok_or_else(|| {
                AppError::Internal(format!(
                    "resolve: resolved canonical entity `{into}` disappeared"
                ))
            })?;
            proposals.stage_input_entity(canonical);
        }
        let mut losing_paths = vec![typed.path.clone()];
        losing_paths.extend(same_as);
        let mut accepted = false;
        for losing in losing_paths {
            let losing = progress.canonical_path(&losing);
            let into = progress.canonical_path(&into);
            if losing == into {
                continue;
            }
            if proposals.input_entity(&losing).is_none() {
                let duplicate = self
                    .ctx
                    .view
                    .entity_by_path(&losing)
                    .await?
                    .ok_or_else(|| {
                        AppError::Internal(format!(
                            "resolve: duplicate entity `{losing}` disappeared"
                        ))
                    })?;
                proposals.stage_input_entity(duplicate);
            }
            state.merged.insert(losing.clone());
            proposals.retarget(&losing, &into);
            if proposals
                .input_entity(&into)
                .is_some_and(|entity| !entity.source_memory_ids.is_empty())
            {
                state.shell_paths.remove(&into);
                state.backed_paths.insert(into.clone());
            }
            proposals.forget(&losing);
            stats.entities_merged += 1;
            if after_first_sweep {
                stats.resolve_merges_after_first_sweep += 1;
            }
            progress
                .commit_resolved_merge(&losing, &into, proposals)
                .await?;
            accepted = true;
        }
        Ok(accepted.then_some(into))
    }

    async fn process_classification_job(
        &self,
        ontology_manager: &crate::memory::pkm::ontology::OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        mode: ClassificationMode,
        proposals: &mut ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        minted: &mut Vec<String>,
    ) -> Result<(), AppError> {
        if mode.stages_entity() {
            proposals.stage_entity(entity.clone());
        }
        // A classification banked by an earlier attempt at this pass is replayed
        // rather than re-derived. Replay must rebuild both the entity and schema
        // proposals because those are not stored in the checkpoint row.
        let mut replay_rejection = None;
        if let Some(banked) = progress.classification(&entity.path) {
            let mut trial = proposals.clone();
            let mut trial_minted = Vec::new();
            if self
                .record_entity_proposal(
                    ontology_manager,
                    entity,
                    &banked,
                    &mut trial,
                    &mut trial_minted,
                )
                .await
            {
                let validation =
                    validate_proposal_projection(&self.ctx, ontology_manager, &trial).await?;
                if validation.is_valid() {
                    *proposals = trial;
                    for path in trial_minted {
                        if !minted.contains(&path) {
                            minted.push(path);
                        }
                    }
                    progress
                        .checkpoint_transition(mode.replay_stage(), &entity.path, proposals)
                        .await?;
                    return Ok(());
                }
                replay_rejection = Some(projection_rejection_details(&validation));
                progress.discard_classification(&entity.path).await?;
            }
        }
        match self
            .classify_entity(
                ontology_manager,
                entity,
                proposals,
                progress,
                minted,
                replay_rejection.as_deref(),
            )
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                proposals.forget(&entity.path);
                progress.discard(&entity.path, mode.discard_reason()).await
            }
            Err(error) => Err(error),
        }
    }

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
            .filter(|entity| is_unbacked_shell(entity))
            .map(|entity| entity.path.clone())
            .collect();

        // Nothing is committed or stamped before Assemble accepts the complete proposal.
        // Entities this pass put into quarantine, from any of the three routes. They must
        // not be released by the reinstatement sweep at the end of the same pass.
        let mut quarantined: HashSet<String> = HashSet::new();
        let mut proposals = ProposalSet::default();
        for entity in &entities {
            proposals.stage_input_entity(entity.clone());
            if entity.entity_id.is_none() && !shell_paths.contains(&entity.path) {
                proposals.stage_entity(entity.clone());
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
                    .is_some_and(|entity| is_unbacked_shell(&entity))
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

#[cfg(test)]
mod tests {
    use super::ClassificationMode;

    #[test]
    fn normal_classification_policy_stays_explicit() {
        let mode = ClassificationMode::Normal;
        assert_eq!(mode.replay_stage(), "classify-replay");
        assert_eq!(
            mode.discard_reason(),
            "classification budget exhausted without a valid projection",
        );
        assert!(!mode.stages_entity());
        assert!(mode.defers_error());
    }

    #[test]
    fn referenced_shell_classification_policy_stays_explicit() {
        let mode = ClassificationMode::ReferencedShell;
        assert_eq!(mode.replay_stage(), "classify-referenced-shell-replay");
        assert_eq!(
            mode.discard_reason(),
            "referenced identity could not be classified",
        );
        assert!(mode.stages_entity());
        assert!(!mode.defers_error());
    }
}
