use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use oxigraph::{sparql::QueryResults, store::Store};
use oxrdf::{Term, Triple};

#[cfg(test)]
use crate::memory::pkm::consolidation::resolve::evidence::{
    IdentityMatch, identity_matches, pair_change_requires_judgment,
    resolution_identity_fingerprint, resolution_pair_fingerprint,
};
use crate::memory::pkm::{
    consolidation::{
        Consolidator, PromptSpec,
        candidates::{RESOLUTION_RETRIEVAL_LIMIT, RankedCandidate, Request, Search, Subject},
        classify::ProposalSet,
        projection_rejection_details,
        resolve::evidence::{
            IdentityConversation, IdentityResolution, ResolutionDecisionContext, ResolveDecision,
            validate_resolution_evidence,
        },
        validate_proposal_projection,
    },
    model::KnowledgeConsolidationEntity,
    ontology::{OntologyManager, individual_iri, path_from_individual, sparql},
};
use crate::{core::error::AppError, tool::registry::ToolFilter};
use frona_harness::{Decision, FailureKind, ManualError, RunOutcome};

/// Evidence and projected validation stay in PKM. Each inspector future owns this
/// state while the outer result reads the same correction diagnostics.
struct ResolutionInspection {
    ctx: Arc<crate::memory::pkm::consolidation::context::ConsolidationContext>,
    ontology: OntologyManager,
    proposals: ProposalSet,
    subject_path: String,
    offered_paths: Vec<String>,
    evidence_context: ResolutionDecisionContext,
    last_failures: Mutex<Vec<String>>,
    correction_count: AtomicUsize,
}

impl ResolutionInspection {
    async fn inspect(
        &self,
        decision: ResolveDecision,
    ) -> Result<Decision<IdentityResolution>, AppError> {
        let ctx = &self.ctx;
        let ontology = &self.ontology;
        let proposal_snapshot = &self.proposals;
        let subject_path = &self.subject_path;
        let offered_paths = &self.offered_paths;
        let offered: HashSet<&str> = offered_paths.iter().map(String::as_str).collect();
        let evidence_context = &self.evidence_context;
        let captured_failures = &self.last_failures;
        let captured_corrections = &self.correction_count;
        let canonical = decision.canonical.trim().to_string();
        let same_as: Vec<String> = decision
            .same_as
            .into_iter()
            .map(|path| path.trim().to_string())
            .collect();
        let mut seen = HashSet::new();
        let mut invalid = Vec::new();
        if !canonical.is_empty()
            && canonical != *subject_path
            && !offered.contains(canonical.as_str())
        {
            invalid.push(canonical.as_str());
        }
        for path in &same_as {
            if path.is_empty()
                || path == &canonical
                || !offered.contains(path.as_str())
                || !seen.insert(path.as_str())
            {
                invalid.push(path.as_str());
            }
        }
        let evidence_errors = validate_resolution_evidence(
            &ResolveDecision {
                canonical: canonical.clone(),
                same_as: same_as.clone(),
                merge_because: decision.merge_because.clone(),
                distinct_because: decision.distinct_because.clone(),
            },
            subject_path,
            &offered,
            evidence_context,
        );
        let keeps_subject_without_merges = canonical == *subject_path && same_as.is_empty();
        let distinct = canonical.is_empty() && same_as.is_empty();
        if distinct && invalid.is_empty() && evidence_errors.is_empty() {
            return Ok(Decision::accept(IdentityResolution::Distinct {
                evidence: serde_json::json!({
                    "merge_because": decision.merge_because,
                    "distinct_because": decision.distinct_because,
                }),
            }));
        }
        if !canonical.is_empty()
            && !same_as.iter().any(|path| path == &canonical)
            && invalid.is_empty()
            && evidence_errors.is_empty()
            && !keeps_subject_without_merges
        {
            let mut trial = proposal_snapshot.clone();
            let mut losing_paths = vec![subject_path.clone()];
            losing_paths.extend(same_as.iter().cloned());
            for losing in losing_paths {
                if losing == canonical {
                    continue;
                }
                trial.retarget(&losing, &canonical);
                trial.forget(&losing);
            }
            let validation = validate_proposal_projection(ctx, ontology, &trial).await?;
            if !validation.is_valid() {
                let details = projection_rejection_details(&validation);
                *captured_failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = vec![details.clone()];
                captured_corrections.fetch_add(1, Ordering::Relaxed);
                let feedback = ctx.llm.reject(
                    PromptSpec::RESOLVE,
                    &[
                        (
                            "proposed",
                            "The proposed identity merge makes the projected graph invalid.",
                        ),
                        ("subject", subject_path.as_str()),
                        ("candidates", details.as_str()),
                    ],
                )?;
                return Ok(Decision::revise(feedback));
            }
            let evidence = serde_json::json!({
                "merge_because": decision.merge_because,
                "distinct_because": decision.distinct_because,
            });
            return Ok(Decision::accept(IdentityResolution::Merge {
                canonical,
                same_as,
                evidence,
            }));
        }
        let mut failures: Vec<String> = invalid
            .iter()
            .map(|path| format!("invalid identity path `{path}`"))
            .collect();
        failures.extend(evidence_errors.iter().cloned());
        *captured_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = failures;
        captured_corrections.fetch_add(1, Ordering::Relaxed);
        let proposed = serde_json::json!({
            "canonical": canonical,
            "same_as": same_as,
            "invalid": invalid,
            "evidence_errors": evidence_errors,
        })
        .to_string();
        let feedback = ctx.llm.reject(
            PromptSpec::RESOLVE,
            &[
                ("proposed", proposed.as_str()),
                ("subject", subject_path.as_str()),
                ("candidates", offered_paths.join(", ").as_str()),
            ],
        )?;
        Ok(Decision::revise(feedback))
    }
}

impl Consolidator {
    /// Retrieve the candidates for one identity against the current entity view.
    /// Candidates are name-similar and not provably type-disjoint. Missing subsumption is
    /// uncertainty, not incompatibility: the same individual can be classified through
    /// different ontology facets. The model makes every identity decision.
    pub(crate) async fn resolution_candidates(
        &self,
        ontology_manager: &OntologyManager,
        store: &Store,
        entity: &KnowledgeConsolidationEntity,
        proposals: &ProposalSet,
        eligible_targets: &HashSet<String>,
        staged_type_delta: &[Triple],
    ) -> Result<Vec<RankedCandidate>, AppError> {
        let user_id = &self.ctx.scope.user_id;
        let effective_ontology = ontology_manager.user_effective_ontology(user_id).await?;
        let px = effective_ontology.prefixes();
        // A candidate's type is its proposal if this pass classified it, else its
        // stored kind. Without this, two mentions of one new entity in the same pass
        // would both be untyped here and could never merge.
        let forced_paths = self.same_as_partners(store, &entity.path)?;
        let request = Request {
            subject: Subject::from_entity(entity),
            eligible_paths: Some(eligible_targets.clone()),
            additional_candidates: Vec::new(),
            forced_paths,
            limit: RESOLUTION_RETRIEVAL_LIMIT as usize,
        };
        Search::new(self.ctx.view.clone())
            .find_candidates(
                request,
                |c| {
                    c.kinds = ontology_manager.normalize_types(
                        &proposals.kinds_for(&c.path, &c.kinds),
                        staged_type_delta,
                    );
                },
                |subject, c| {
                    let ours: Vec<String> =
                        subject.kinds.iter().map(|kind| px.expand(kind)).collect();
                    let theirs: Vec<String> = c.kinds.iter().map(|kind| px.expand(kind)).collect();
                    ontology_type_affinity(store, &ours, &theirs, px)
                },
            )
            .await
    }

    /// The entity paths the reasoned closure says this entity is the same entity as.
    ///
    /// Only `owl:sameAs` reaching another *entity* counts: the closure also holds the
    /// reflexive `sameAs` `eq-ref` mints for every individual, and identity onto
    /// anything outside the KB namespace is not an entity this pass can merge into.
    fn same_as_partners(&self, store: &Store, path: &str) -> Result<Vec<String>, AppError> {
        let me = individual_iri(path);
        let query = format!(
            "SELECT ?o WHERE {{ <{me}> owl:sameAs ?o . FILTER(isIRI(?o)) \
             FILTER(STR(?o) != \"{me}\") }}"
        );
        let QueryResults::Solutions(sols) = sparql::query(store, &query, &self.prefixes)? else {
            return Ok(Vec::new());
        };
        let mut out: Vec<String> = sols
            .flatten()
            .filter_map(|s| match s.get("o") {
                Some(Term::NamedNode(n)) => path_from_individual(n.as_str()),
                _ => None,
            })
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Ambiguous resolution: the model decides same-vs-distinct over the similarity-filtered
    /// candidates, in a NON-PERSISTENT tool conversation (graph and entity-view
    /// exploration via consolidation-scoped tools). External validation: only a path that was
    /// actually offered is accepted - a hallucinated `same_as` is fed back for a
    /// correction. An evidence-backed empty verdict means distinct; exhausting the
    /// correction budget remains explicitly unresolved and never applies a merge.
    pub(crate) async fn adjudicate_identity(
        &self,
        ontology_manager: &OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        candidates: &[RankedCandidate],
        decision_context: &ResolutionDecisionContext,
        proposals: &ProposalSet,
    ) -> Result<IdentityConversation, AppError> {
        let paths: Vec<String> = candidates.iter().map(|c| c.entity.path.clone()).collect();

        let vars = [
            ("path", entity.path.as_str()),
            ("name", entity.name.as_str()),
            (
                "aliases",
                if decision_context.subject_fields.aliases.is_empty() {
                    "(none)"
                } else {
                    &decision_context.subject_fields.aliases
                },
            ),
            (
                "description",
                if entity.description.is_empty() {
                    "(none)"
                } else {
                    &entity.description
                },
            ),
            ("kind", decision_context.kinds_display.as_str()),
            (
                "identity_evidence",
                decision_context.identity_evidence.as_str(),
            ),
            (
                "assertions",
                if decision_context.subject_fields.assertions.is_empty() {
                    "(none)"
                } else {
                    &decision_context.subject_fields.assertions
                },
            ),
            ("candidates", &decision_context.candidate_block),
        ];
        let overlay = self.tool_overlay(ontology_manager, proposals).await?;
        let evidence_context = decision_context
            .clone()
            .with_tool_visible_state(&entity.path, &overlay.entities);
        let tools =
            crate::memory::pkm::consolidation::tools::ontology::build_ontology_tools_with_overlay(
                ontology_manager.clone(),
                &self.ctx,
                self.prefixes.clone(),
                Some(overlay),
                crate::memory::pkm::consolidation::tools::ontology::OntologyToolProfile::Resolve,
            );
        let harness = self
            .ctx
            .llm
            .sdk(
                &self.ctx.scope.agent_id,
                &[ToolFilter::AllowList(&[])],
                &tools,
            )
            .await?;
        let inspection = Arc::new(ResolutionInspection {
            ctx: self.ctx.clone(),
            ontology: ontology_manager.clone(),
            proposals: proposals.clone(),
            subject_path: entity.path.clone(),
            offered_paths: paths,
            evidence_context,
            last_failures: Mutex::new(Vec::new()),
            correction_count: AtomicUsize::new(0),
        });
        let captured = inspection.clone();
        let refined = harness
            .run(PromptSpec::RESOLVE, &vars)?
            .exploration_turns(self.config.pkm_consolidation_max_tool_turns)
            .structured::<ResolveDecision>()
            .submission_attempts(self.config.pkm_consolidation_max_submissions)
            .inspect(move |decision| {
                let inspection = captured.clone();
                async move { inspection.inspect(decision).await }
            })
            .await;
        let corrections = inspection.correction_count.load(Ordering::Relaxed);
        // An unresolved investigation is not evidence of distinct identities.
        // Operational failures remain retryable rather than becoming domain decisions.
        let decision = match refined {
            Ok(decision) => decision,
            Err(incomplete) => {
                tracing::warn!(reason = %incomplete.reason, outcome = ?incomplete.report.outcome,
                    "identity resolution incomplete");
                match incomplete.reason {
                    ManualError::SubmissionAttemptsExhausted { .. } if corrections > 0 => {
                        IdentityResolution::Unresolved {
                            diagnostic: serde_json::json!({
                                "reason": "model_exhausted_without_valid_evidence",
                                "validation_errors": inspection.last_failures.lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
                            }),
                            pair_count: inspection.offered_paths.len(),
                        }
                    }
                    reason
                        if matches!(
                            incomplete.report.outcome,
                            Some(RunOutcome::Incomplete {
                                kind: FailureKind::Cancelled,
                                ..
                            })
                        ) =>
                    {
                        return Err(AppError::Inference(Box::new(
                            frona_harness::inference::InferenceError::Cancelled(reason.to_string()),
                        )));
                    }
                    reason => {
                        return Err(AppError::Internal(format!(
                            "identity resolution incomplete: {reason}"
                        )));
                    }
                }
            }
        };
        Ok(IdentityConversation {
            decision,
            corrections,
        })
    }
}

/// Identity is impossible only when the ontology proves it: either asserted class (or an
/// ancestor of it) is disjoint with an asserted class (or ancestor) on the other entity.
/// No known relationship between the class trees remains an open-world uncertainty.
pub(super) fn types_provably_disjoint(
    store: &Store,
    ours: &[String],
    theirs: &[String],
    prefixes: &crate::memory::pkm::ontology::PrefixMap,
) -> bool {
    ours.iter().any(|ours| {
        theirs.iter().any(|theirs| {
            sparql::ask(
                store,
                &format!(
                    "ASK {{ \
               {{ <{ours}> <http://www.w3.org/2000/01/rdf-schema#subClassOf>* ?a . \
                  <{theirs}> <http://www.w3.org/2000/01/rdf-schema#subClassOf>* ?b . \
                  ?a <http://www.w3.org/2002/07/owl#disjointWith> ?b }} \
               UNION \
               {{ <{ours}> <http://www.w3.org/2000/01/rdf-schema#subClassOf>* ?a . \
                  <{theirs}> <http://www.w3.org/2000/01/rdf-schema#subClassOf>* ?b . \
                  ?b <http://www.w3.org/2002/07/owl#disjointWith> ?a }} \
             }}"
                ),
                prefixes,
            )
            .unwrap_or(false)
        })
    })
}

pub(super) fn ontology_type_affinity(
    store: &Store,
    ours: &[String],
    theirs: &[String],
    prefixes: &crate::memory::pkm::ontology::PrefixMap,
) -> Option<u8> {
    if types_provably_disjoint(store, ours, theirs, prefixes) {
        return None;
    }
    if ours.iter().any(|ours| theirs.contains(ours)) {
        return Some(3);
    }
    let related = ours.iter().any(|ours| {
        theirs.iter().any(|theirs| {
            sparql::ask(
                store,
                &format!(
                    "ASK {{ {{ <{ours}> rdfs:subClassOf+ <{theirs}> }} UNION \
                    {{ <{theirs}> rdfs:subClassOf+ <{ours}> }} }}"
                ),
                prefixes,
            )
            .unwrap_or(false)
        })
    });
    Some(if related { 2 } else { 0 })
}

#[cfg(test)]
mod tests;
