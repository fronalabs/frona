//! Resolve the identity worklist and durably apply portable phase decisions.
use super::progress::ConsolidationProgress;
use crate::memory::pkm::consolidation::ResolveOutcome;
use crate::memory::pkm::model::SELF_ENTITY_PATH;
use crate::{core::error::AppError, memory::pkm::consolidation::Consolidator};
use frona_pkm::classify::ProposalSet;
use frona_pkm::resolve::{self, Resolve, ResolvePrompts};
use std::collections::HashSet;
impl Consolidator {
    pub(super) fn resolution_phase(&self) -> Result<Resolve<'static>, AppError> {
        let mut prompts = ResolvePrompts::default();
        let llm = &self.ctx.llm;
        prompts.defaults = llm.prompt_defaults();
        prompts.system = llm.prompt_source("pkm/resolve/system.md", &prompts.system)?;
        prompts.input = llm.prompt_source("pkm/resolve/input.md", &prompts.input)?;
        prompts.reject = llm.prompt_source("pkm/resolve/reject.md", &prompts.reject)?;
        Ok(Resolve::new(llm.harness())
            .candidate_retrieval(std::sync::Arc::new(self.ctx.view.clone()))
            .prompts(prompts)
            .submission_attempts(self.config.pkm_consolidation_max_submissions)
            .exploration_turns(self.config.pkm_consolidation_max_tool_turns))
    }
}

pub(super) struct ResolveWorkState {
    backed_paths: HashSet<String>,
    shell_paths: HashSet<String>,
    merged: HashSet<String>,
    incremental_started: bool,
}

impl ResolveWorkState {
    pub(super) fn new(
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

impl Consolidator {
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
        let phase = self.resolution_phase()?;
        let output = phase
            .run_with(
                frona_pkm::resolve::ResolveInput {
                    path: path.to_string(),
                    graph: self.ctx.view.portable_snapshot(&self.ctx.repo).await?,
                    ontology: self.ontology.portable(&self.ctx.scope.user_id).await?,
                    proposals: proposals.clone(),
                    eligible_targets: state.backed_paths.clone(),
                    reconsider: after_first_sweep,
                    prior_identity_fingerprint: progress
                        .resolution_fingerprint(path)
                        .map(str::to_owned),
                    prior_pair_fingerprints: progress.resolution_pair_fingerprints(),
                },
                |run| self.ctx.llm.configure(run),
            )
            .await?;
        stats.resolve_fingerprint_skips += output.stats.fingerprint_skips;
        stats.resolve_identity_state_changes += output.stats.identity_state_changes;
        stats.resolve_candidate_evaluations += output.stats.candidate_evaluations;
        stats.resolve_identity_pair_weakenings += output.stats.pair_weakenings;
        stats.resolve_identity_pair_changes += output.stats.pair_changes;
        stats.resolve_decision_attempts += output.stats.decision_attempts;
        stats.resolve_conversations += output.stats.conversations;
        stats.resolve_evidence_corrections += output.corrections;
        if after_first_sweep {
            if output.stats.candidate_evaluations > 0 && !state.incremental_started {
                state.incremental_started = true;
                stats.resolve_sweeps += 1;
            }
            stats.resolve_candidate_evaluations_after_first_sweep +=
                output.stats.candidate_evaluations;
            stats.resolve_reconsiderations += output.stats.decision_attempts;
            stats.resolve_reconsideration_conversations += output.stats.conversations;
        }
        let Some(resolution) = output.decision else {
            return Ok(None);
        };
        let identity_fingerprint = output.identity_fingerprint.ok_or_else(|| {
            AppError::Internal(
                "resolve returned a decision without its identity fingerprint".into(),
            )
        })?;
        *proposals = output.prepared_proposals;
        progress.remember_resolution_pairs(output.pair_fingerprints);

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
            proposals.stage_input_entity(canonical.into());
        }
        let mut losing_paths = vec![path.to_string()];
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
                proposals.stage_input_entity(duplicate.into());
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
}
