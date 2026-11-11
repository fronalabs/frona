//! Resolve one identity against an immutable graph snapshot. The host owns the
//! worklist, canonical-path/checkpoint skips, atomic merges, and subsequent sweeps.
mod evidence;
mod prompts;
mod run;
use crate::prompt::prompt_evidence;
use crate::{
    Error, GraphSnapshot, Phase,
    classify::ProposalSet,
    ontology::{Ontology, PrefixMap},
    projection::is_unbacked_shell,
};
use evidence::ResolutionDecisionContext;
pub use evidence::{
    IdentityMatch, IdentityResolution, identity_matches, pair_change_requires_judgment,
    resolution_identity_fingerprint, resolution_pair_fingerprint, resolution_pair_key,
};
use frona_harness::{Harness, Run, Text};
pub use prompts::ResolvePrompts;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

/// Supply the complete validation graph and the set of memory-backed identities
/// eligible to survive a merge. This excludes unbacked shells, as in the server.
#[derive(Clone)]
pub struct ResolveInput {
    pub path: String,
    pub graph: GraphSnapshot,
    pub ontology: Ontology,
    pub proposals: ProposalSet,
    pub eligible_targets: HashSet<String>,
    pub reconsider: bool,
    pub prior_identity_fingerprint: Option<String>,
    pub prior_pair_fingerprints: BTreeMap<String, String>,
}
/// No mutations are persisted. `None` means no investigation was necessary, never
/// a distinct verdict. Fingerprints are returned for the host to bank atomically.
pub struct ResolveOutput {
    pub decision: Option<IdentityResolution>,
    pub proposals: ProposalSet,
    pub identity_fingerprint: Option<String>,
    pub pair_fingerprints: Vec<(String, String)>,
    pub corrections: usize,
}
pub struct Resolve<'a> {
    harness: Harness<'a>,
    prompts: ResolvePrompts,
    max_submissions: usize,
    max_tool_turns: usize,
}
impl<'a> Resolve<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: ResolvePrompts::default(),
            max_submissions: 10,
            max_tool_turns: 20,
        }
    }
    pub fn prompts(mut self, value: ResolvePrompts) -> Self {
        self.prompts = value;
        self
    }
    pub fn submission_attempts(mut self, value: usize) -> Self {
        self.max_submissions = value;
        self
    }
    pub fn exploration_turns(mut self, value: usize) -> Self {
        self.max_tool_turns = value;
        self
    }
    pub async fn run(&self, input: ResolveInput) -> Result<ResolveOutput, Error> {
        self.run_with(input, |run| run).await
    }
    pub async fn run_with<'run, F>(
        &'run self,
        input: ResolveInput,
        configure: F,
    ) -> Result<ResolveOutput, Error>
    where
        'a: 'run,
        F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
    {
        let ResolveInput {
            path,
            graph,
            ontology,
            mut proposals,
            eligible_targets,
            reconsider,
            prior_identity_fingerprint,
            prior_pair_fingerprints,
        } = input;
        let execution = ResolveRun {
            ctx: Arc::new(graph),
            prefixes: ontology.prefixes(),
            harness: &self.harness,
            prompts: self.prompts.clone(),
            max_submissions: self.max_submissions,
            max_tool_turns: self.max_tool_turns,
        };
        let empty = |proposals| ResolveOutput {
            decision: None,
            proposals,
            identity_fingerprint: None,
            pair_fingerprints: vec![],
            corrections: 0,
        };
        if path == crate::model::SELF_ENTITY_PATH {
            return Ok(empty(proposals));
        }
        let Some(stored) = execution
            .ctx
            .entity_by_path_with(&proposals.entity_draft(), &path)?
        else {
            return Ok(empty(proposals));
        };
        if stored.path != path {
            return Ok(empty(proposals));
        }
        let delta = ontology
            .plan_schema(&execution.ctx.scope_id, &proposals.proposed_edits)
            .await?
            .triples;
        let kinds = ontology.normalize_types(&proposals.kinds_for(&path, &stored.kinds), &delta);
        if !is_unbacked_shell(&stored) && kinds.iter().all(|kind| kind.trim().is_empty()) {
            return Ok(empty(proposals));
        }
        let mut typed = proposals.project_entity(stored);
        typed.kinds = kinds;
        typed.rederive_search();
        // The server stages subjects during its outer worklist. A standalone
        // invocation must stage the same full row before validating trial merges.
        proposals.stage_input_entity(typed.clone());
        let keys = proposals.provisional_has_keys();
        let inverse = proposals.provisional_inverse_functional_properties();
        let fingerprint =
            resolution_identity_fingerprint(&typed, &keys, &inverse, &execution.prefixes);
        if reconsider && prior_identity_fingerprint.as_deref() == Some(&fingerprint) {
            return Ok(empty(proposals));
        }
        let overlay =
            crate::tools::ontology::overlay(&ontology, &execution.ctx, &proposals, usize::MAX)?;
        let reasoned = ontology
            .reason_user_with_proposed(
                &execution.ctx.scope_id,
                &proposals.proposed_edits,
                &overlay.abox,
            )
            .await?;
        let mut candidates = execution
            .resolution_candidates(
                &ontology,
                &reasoned.reasoned.store,
                &typed,
                &proposals,
                &eligible_targets,
                &delta,
            )
            .await?;
        let mut matches: BTreeMap<_, _> = candidates
            .iter()
            .map(|c| {
                (
                    c.entity.path.clone(),
                    identity_matches(&typed, &c.entity, &keys, &inverse, &execution.prefixes),
                )
            })
            .collect();
        let mut pairs = Vec::new();
        if reconsider {
            candidates.retain(|candidate| {
                let evidence = matches
                    .get(&candidate.entity.path)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let key = resolution_pair_key(&typed.path, &candidate.entity.path);
                let next = resolution_pair_fingerprint(&typed, &candidate.entity, evidence);
                let old = prior_pair_fingerprints.get(&key).map(String::as_str);
                let judge = !(evidence.is_empty() && old.is_none())
                    && pair_change_requires_judgment(old, &next);
                if !judge && old != Some(&next) {
                    pairs.push((key, next));
                }
                judge
            });
        } else {
            candidates.sort_by_key(|c| matches.get(&c.entity.path).is_none_or(Vec::is_empty));
        }
        candidates.truncate(crate::candidates::RESOLUTION_PROMPT_LIMIT);
        matches.retain(|path, _| candidates.iter().any(|c| &c.entity.path == path));
        for candidate in &candidates {
            if proposals.input_entity(&candidate.entity.path).is_none()
                && let Some(entity) = execution.ctx.entity_by_path(&candidate.entity.path)?
            {
                proposals.stage_input_entity(entity);
            }
        }
        let candidate_evidence = candidates
            .iter()
            .map(|c| {
                (
                    c.entity.path.clone(),
                    proposals
                        .input_entity(&c.entity.path)
                        .map(|e| prompt_evidence(&e.identity_evidence))
                        .unwrap_or_else(|| "(none)".into()),
                )
            })
            .collect();
        let context = ResolutionDecisionContext::new(
            &typed,
            &candidates,
            &candidate_evidence,
            &matches,
            &execution.prefixes,
        );
        pairs.extend(context.pair_fingerprints.clone());
        let (decision, corrections) = if candidates.is_empty() {
            (
                IdentityResolution::Distinct {
                    evidence: serde_json::json!({"reason":if reconsider{"no_changed_identity_candidates"}else{"no_identity_candidates"}}),
                },
                0,
            )
        } else {
            let result = execution
                .adjudicate_identity(
                    &ontology,
                    &typed,
                    &candidates,
                    &context,
                    &proposals,
                    configure,
                )
                .await?;
            (result.decision, result.corrections)
        };
        if let IdentityResolution::Merge {
            canonical, same_as, ..
        } = &decision
        {
            if proposals.input_entity(canonical).is_none()
                && let Some(entity) = execution.ctx.entity_by_path(canonical)?
            {
                proposals.stage_input_entity(entity);
            }
            for losing in std::iter::once(&typed.path).chain(same_as.iter()) {
                if losing != canonical {
                    proposals.retarget(losing, canonical);
                    proposals.forget(losing);
                }
            }
        }
        Ok(ResolveOutput {
            decision: Some(decision),
            proposals,
            identity_fingerprint: Some(fingerprint),
            pair_fingerprints: pairs,
            corrections,
        })
    }
}
impl Phase for Resolve<'_> {
    type Input = ResolveInput;
    type Output = ResolveOutput;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        Resolve::run(self, input).await
    }
}
struct ResolveRun<'a, 'h> {
    ctx: Arc<GraphSnapshot>,
    prefixes: PrefixMap,
    harness: &'a Harness<'h>,
    prompts: ResolvePrompts,
    max_submissions: usize,
    max_tool_turns: usize,
}

#[cfg(test)]
mod tests;
