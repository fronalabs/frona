use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use tracing::warn;

use crate::memory::pkm::{
    consolidation::{
        ConsolidationProgress, Consolidator, PromptIds, PromptSpec, bad_term_feedback,
        candidates::{RESOLUTION_PROMPT_LIMIT, Request, Search, Subject},
        classify::{
            claim_automatic_identity_discovery,
            proposal::{
                ATTRIBUTE_CANDIDATES, AttributeDecisions, AttributeMapping, Classification,
                EVIDENCE_VOCAB_HITS, EntityProposal, NewEntity, ProposalSet, RelationMapping,
                accept_mints, attribute_edits, classification_edits, render_value, search_terms,
            },
        },
        projection_rejection_details, prompt_evidence, validate_proposal_projection,
    },
    model::{
        EntityCategory, KnowledgeConsolidationEntity, KnowledgeEntity, KnowledgeEntityLink,
        LinkOrigin,
    },
    ontology::{OntologyManager, SchemaEdit},
};
use crate::{core::error::AppError, tool::registry::ToolFilter};
use frona_harness::{Decision, FailureKind, ManualError, RunOutcome};

/// Application-owned validation state. The inspector and research tools share the
/// same diagnostics, while each async inspection owns its state at Send boundaries.
struct ClassificationInspection {
    consolidator: Consolidator,
    ontology: OntologyManager,
    entity: KnowledgeConsolidationEntity,
    proposals: ProposalSet,
    prompt_ids: PromptIds,
    existing: HashSet<String>,
    overlay: Arc<crate::memory::pkm::consolidation::tools::ontology::OntologyToolOverlay>,
    requires_page_shape: bool,
    attempts: AtomicUsize,
    failures: AtomicUsize,
    details: std::sync::Mutex<Vec<String>>,
    challenged: AtomicBool,
}

impl ClassificationInspection {
    async fn inspect(
        &self,
        candidate: Classification,
    ) -> Result<Decision<Classification>, AppError> {
        let ctx = &self.consolidator.ctx;
        let prefixes = &self.consolidator.prefixes;
        let ontology_manager = &self.ontology;
        let entity = &self.entity;
        let identity_page = entity;
        let proposals_snapshot = &self.proposals;
        let validation_prompt_ids = &self.prompt_ids;
        let existing = &self.existing;
        let diagnostic_store = &self.overlay.diagnostics;
        let discovery_calls = &self.overlay.tool_calls;
        let (attempts, failures, details, challenged) = (
            &self.attempts,
            &self.failures,
            &self.details,
            &self.challenged,
        );
        let requires_page_shape = self.requires_page_shape;
        attempts.fetch_add(1, Ordering::Relaxed);
        if candidate.entity.name.trim().is_empty() && requires_page_shape {
            failures.fetch_add(1, Ordering::Relaxed);
            let feedback = "The canonical entity name cannot be empty. Re-submit the complete classification with entity, classes, relations, attributes, new_entities, declarations, has_keys, and inverse_functional_properties.".to_string();
            details
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(feedback.clone());
            return Ok(Decision::revise(feedback));
        }
        let classes = candidate.class_curies();
        if classes.is_empty() {
            failures.fetch_add(1, Ordering::Relaxed);
            details
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("submission contained no classes".into());
            return Ok(Decision::abandon("classification contained no classes"));
        }
        // A classification made without looking for the entity can be
        // internally valid and still type a duplicate mention incorrectly.
        // Give the first otherwise-formed submission one automatic identity
        // discovery. Multiple plausible entities require an explicit second
        // submission; the challenge is intentionally one-shot.
        if claim_automatic_identity_discovery(discovery_calls, challenged) {
            let mut subject_page = identity_page.clone();
            subject_page.kinds = classes.clone();
            if !candidate.entity.name.trim().is_empty() {
                subject_page.name = candidate.entity.name.trim().to_string();
                subject_page.description = candidate.entity.description.clone();
                subject_page.aliases = candidate
                    .entity
                    .aliases
                    .iter()
                    .map(|alias| alias.trim().to_string())
                    .filter(|alias| !alias.is_empty())
                    .collect();
                subject_page.rederive_search();
            }
            let candidates = Search::new(ctx.view.clone())
                .find_candidates(
                    Request {
                        subject: Subject::from_entity(&subject_page),
                        eligible_paths: None,
                        additional_candidates: Vec::new(),
                        forced_paths: Vec::new(),
                        limit: RESOLUTION_PROMPT_LIMIT,
                    },
                    |_| {},
                    |_, _| Some(0),
                )
                .await?;
            if candidates.len() > 1 {
                failures.fetch_add(1, Ordering::Relaxed);
                let choices = candidates
                    .iter()
                    .map(|candidate| {
                        format!(
                            "- {} — {} [{}]: {}",
                            candidate.entity.path,
                            candidate.entity.name,
                            candidate.entity.kinds.join(", "),
                            candidate.entity.description,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let feedback = format!(
                    "Automatic identity discovery found multiple plausible existing entities:\n{choices}\nConfirm whether this mention is distinct or revise the complete classification using the matching identity. Re-submit all fields; the next complete submission will proceed through normal ontology validation."
                );
                details
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(feedback.clone());
                return Ok(Decision::revise(feedback));
            }
        }
        if let Some(feedback) = candidate.declaration_feedback(existing, prefixes) {
            failures.fetch_add(1, Ordering::Relaxed);
            details
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(feedback.clone());
            return Ok(Decision::revise(feedback));
        }
        if let Some(feedback) = candidate.identity_marker_feedback(entity) {
            failures.fetch_add(1, Ordering::Relaxed);
            details
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(feedback.clone());
            return Ok(Decision::revise(feedback));
        }
        if let Some(feedback) =
            bad_term_feedback(ctx, PromptSpec::CLASSIFY, prefixes, candidate.terms())?
        {
            failures.fetch_add(1, Ordering::Relaxed);
            details
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(feedback.clone());
            return Ok(Decision::revise(feedback));
        }
        let mut validation_candidate = candidate.clone();
        validation_candidate.expand_prompt_ids(validation_prompt_ids)?;
        // Compile every half of the response before admission so the gate also
        // validates relation and attribute declarations.
        let mut trial = proposals_snapshot.clone();
        let fresh = self
            .consolidator
            .mint_entities(entity, &validation_candidate.new_entities, &mut trial)
            .await;
        let staged_targets: Vec<String> = trial.staged_entities.keys().cloned().collect();
        let mut edits = classification_edits(&validation_candidate);
        let (relation_edits, rekeys) = self
            .consolidator
            .relation_proposals(ontology_manager, &validation_candidate.relations)
            .await;
        let (attribute_edits, attr_rekeys, promoted) = self
            .consolidator
            .attribute_proposals(
                ontology_manager,
                entity,
                &validation_candidate.attributes,
                &fresh,
                &staged_targets,
            )
            .await;
        edits.extend(relation_edits);
        edits.extend(attribute_edits);
        if !validation_candidate.entity.name.trim().is_empty() {
            trial
                .entity_shapes
                .insert(entity.path.clone(), validation_candidate.entity.clone());
        }
        trial.record(
            &entity.path,
            EntityProposal {
                classes: classes.clone(),
                edits,
                rekeys,
                attr_rekeys,
                promoted,
                promoted_sources: HashMap::new(),
                retracted: Vec::new(),
                has_keys: validation_candidate.has_keys.clone(),
                inverse_functional_properties: validation_candidate
                    .inverse_functional_properties
                    .clone(),
            },
        );
        let validation = validate_proposal_projection(ctx, ontology_manager, &trial).await?;
        if validation.is_valid() {
            return Ok(Decision::accept(candidate));
        }
        failures.fetch_add(1, Ordering::Relaxed);
        {
            let mut held = diagnostic_store
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            held.clear();
            held.extend(
                validation
                    .diagnostics
                    .iter()
                    .cloned()
                    .map(|d| (d.id.clone(), d)),
            );
        }
        let detail = projection_rejection_details(&validation);
        details
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(detail.clone());
        let feedback = ctx.llm.reject(
            PromptSpec::CLASSIFY,
            &[
                ("violations", detail.as_str()),
                ("class", classes.join(", ").as_str()),
            ],
        )?;
        Ok(Decision::revise(feedback))
    }
}

impl Consolidator {
    /// Classify **and its schema-satisfaction loop** as a non-persistent tool
    /// conversation the Classify stage drives itself. The model proposes a class (exploring
    /// the schema/graph via the ontology tools); the SYSTEM validates each submission
    /// with the cumulative projection validator; on a violation the reasoner
    /// errors are fed back - through our own `classify/reject.md` prompt - for a
    /// revision, until it validates or the response-attempt cap runs out. Accepted
    /// proposals are banked and staged for the application's later graph commit.
    /// Rejected classifications are discarded without hiding durable facts.
    /// Returns whether the entity was typed. Execution or inspector failures are errors,
    /// including interruption after a rejected candidate; the entity remains retryable.
    /// Semantic exhaustion/empty abandonment is an explicit domain rejection.
    pub(crate) async fn classify_entity(
        &self,
        ontology_manager: &OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        proposals: &mut ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        minted_entities: &mut Vec<String>,
        initial_rejection: Option<&str>,
    ) -> Result<bool, AppError> {
        let user_id = &self.ctx.scope.user_id;
        let mut facts = self
            .ctx
            .repo
            .current_memories_for_entity(user_id, &entity.path)
            .await?;
        let pending = self
            .ctx
            .repo
            .memories_by_ids(user_id, &entity.source_memory_ids)
            .await?;
        for memory in pending {
            if !facts.iter().any(|held| held.id == memory.id) {
                facts.push(memory);
            }
        }
        let prompt_ids = PromptIds::new("f", facts.iter().map(|memory| memory.id.clone()));
        // Facts carry a request-local ID so a mint can cite the one that stated it, and the new entity
        // shares that memory instead of being a name with nothing behind it.
        let mut fact_lines = String::new();
        for memory in &facts {
            let mut entities = self
                .ctx
                .repo
                .memory_entity_paths(user_id, &memory.id)
                .await?;
            entities.extend(proposals.memory_paths(&memory.id));
            entities.sort();
            entities.dedup();
            fact_lines.push_str(&format!(
                "- [{}] entities={} evidence={} {}\n",
                prompt_ids.local(&memory.id),
                serde_json::to_string(&entities).unwrap_or_else(|_| "[]".into()),
                prompt_evidence(&memory.evidence),
                memory.content,
            ));
        }
        let minted = ontology_manager
            .catalog(user_id)
            .await
            .unwrap_or_default()
            .classes
            .join(", ");
        // The entity's stated (asserted, free-text) relations, for the model to map to
        // CURIE object properties.
        let links = self
            .ctx
            .repo
            .links_from_entity(user_id, &entity.path)
            .await
            .unwrap_or_default();
        let relation_lines: String = links
            .iter()
            .filter(|l| l.origin != LinkOrigin::Inferred)
            .map(|l| format!("- \"{}\" → {}\n", l.relation, l.to_entity_path))
            .collect();
        let (attribute_lines, evidence) = self
            .classification_evidence(ontology_manager, entity, &links, progress)
            .await;
        let evidence_text =
            serde_json::to_string_pretty(&evidence).unwrap_or_else(|_| "(unavailable)".into());
        progress.persist().await?;
        frona_harness::inference::trace::record_stage_state(
            "consolidation-stage-state",
            "classify-evidence",
            &entity.path,
            &evidence,
        );
        let contribution_text = progress
            .entity_row(&entity.path)
            .map(|row| {
                serde_json::to_string_pretty(
                    &row.contributions
                        .iter()
                        .map(|item| {
                            serde_json::json!({
                                "name": item.name,
                                "description": item.description,
                                "aliases": item.aliases,
                                "attributes": item.attributes,
                                "existing_only": item.existing_only,
                                "occurrence_count": item.occurrence_count,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .transpose()
            .unwrap_or(None)
            .unwrap_or_else(|| "[]".into());
        let identity_evidence = prompt_evidence(&entity.identity_evidence);

        let vars = [
            ("name", entity.name.as_str()),
            (
                "description",
                if entity.description.is_empty() {
                    "(none)"
                } else {
                    &entity.description
                },
            ),
            (
                "facts",
                if fact_lines.is_empty() {
                    "(none)\n"
                } else {
                    &fact_lines
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
            (
                "attributes",
                if attribute_lines.is_empty() {
                    "(none)\n"
                } else {
                    &attribute_lines
                },
            ),
            ("minted", &minted),
            ("evidence", &evidence_text),
            ("contributions", &contribution_text),
            ("identity_evidence", &identity_evidence),
        ];

        let overlay = self.tool_overlay(ontology_manager, proposals).await?;
        let tools =
            crate::memory::pkm::consolidation::tools::ontology::build_ontology_tools_with_overlay(
                ontology_manager.clone(),
                &self.ctx,
                self.prefixes.clone(),
                Some(overlay.clone()),
                crate::memory::pkm::consolidation::tools::ontology::OntologyToolProfile::Classify,
            );
        if self.config.pkm_consolidation_max_submissions == 0 {
            return Err(AppError::Internal(
                "classification: max_submissions must be at least one".into(),
            ));
        }
        let rejection = initial_rejection.map(|rejection| format!(
            "\n\nA previously accepted checkpoint proposal is no longer valid against the reconstructed graph. Revise it before it can be staged:\n{rejection}"
        )).unwrap_or_default();
        let harness = self
            .ctx
            .llm
            .sdk(
                &self.ctx.scope.agent_id,
                &[ToolFilter::AllowList(&[])],
                &tools,
            )
            .await?;

        // Propose → external reasoner check → feed violations back (our prompt) → revise.
        // Nothing is kept from a rejected round: a classification that violates the
        // ontology is not a partial answer, it is the wrong answer.
        let px = self.prefixes.clone();
        let catalog = ontology_manager.catalog(user_id).await.unwrap_or_default();
        let existing: HashSet<String> = catalog
            .classes
            .into_iter()
            .chain(catalog.object_properties)
            .chain(catalog.data_properties)
            .collect();
        let inspection = Arc::new(ClassificationInspection {
            consolidator: self.clone(),
            ontology: ontology_manager.clone(),
            entity: entity.clone(),
            proposals: proposals.clone(),
            prompt_ids: prompt_ids.clone(),
            existing,
            overlay,
            requires_page_shape: progress
                .entity_row(&entity.path)
                .is_some_and(|row| !row.contributions.is_empty()),
            attempts: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            details: std::sync::Mutex::new(Vec::new()),
            challenged: AtomicBool::new(false),
        });
        let run = harness
            .run_with_input_suffix(PromptSpec::CLASSIFY, &vars, &rejection)?
            .exploration_turns(self.config.pkm_consolidation_max_tool_turns)
            .structured::<Classification>()
            .submission_attempts(self.config.pkm_consolidation_max_submissions);
        let report = run.report();
        let inspector = inspection.clone();
        let refined = run
            .inspect(move |candidate| {
                let inspector = inspector.clone();
                async move { inspector.inspect(candidate).await }
            })
            .await;
        let execution = report.snapshot();
        let request_count = execution.logical_calls();
        let diagnostic = serde_json::json!({
            "evidence": evidence,
            "request_count": request_count,
            "one_request": request_count == 1,
            "fallback_investigation_requests": request_count.saturating_sub(1),
            "validation": {
                "attempts": inspection.attempts.load(Ordering::Relaxed),
                "failures": inspection.failures.load(Ordering::Relaxed),
                "failure_batches": inspection.details
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            },
        });
        progress
            .bank_classify_diagnostic(&entity.path, diagnostic.clone())
            .await?;
        frona_harness::inference::trace::record_stage_state(
            "consolidation-stage-state",
            "classify-diagnostic",
            &entity.path,
            &diagnostic,
        );
        // A rejected semantic candidate is distinct from interrupted execution.
        // Neither can enter the accepted checkpoint/commit path.
        match refined {
            Err(incomplete) => {
                warn!(reason = %incomplete.reason, outcome = ?incomplete.report.outcome,
                    "classification refinement incomplete");
                match incomplete.reason {
                    ManualError::Abandoned(_) => Ok(false),
                    ManualError::SubmissionAttemptsExhausted { .. }
                        if inspection.attempts.load(Ordering::Relaxed) > 0 =>
                    {
                        Ok(false)
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
                        Err(AppError::Inference(Box::new(
                            frona_harness::inference::InferenceError::Cancelled(reason.to_string()),
                        )))
                    }
                    reason => Err(AppError::Internal(format!(
                        "classification incomplete: {reason}"
                    ))),
                }
            }
            Ok(mut candidate) => {
                candidate.expand_prompt_ids(&prompt_ids)?;
                // House spelling before anything derives from it - including the bank, so a
                // resumed pass proposes the same terms this one did rather than the raw ones.
                let candidate = candidate.repaired(&px);
                // Bank it before anything else can fail: the conversation is paid for,
                // and a later stage dying must not make the pass buy it twice.
                progress
                    .bank_classification(&entity.path, &candidate)
                    .await?;
                // The "Map" half of classify - mints, relations and attributes - is shared
                // with the resume path that replays a banked classification, so the two
                // cannot drift apart.
                let recorded = self
                    .record_entity_proposal(
                        ontology_manager,
                        entity,
                        &candidate,
                        proposals,
                        minted_entities,
                    )
                    .await;
                progress
                    .checkpoint_transition("classify", &entity.path, proposals)
                    .await?;
                Ok(recorded)
            }
        }
    }

    /// The entity's free-text attributes, each with the entities its **value** could be naming.
    ///
    /// Retrieval is code, judgment is the model's: a value only ever *looks* like an
    /// entity, and "Example Corp" matching an entity called Example Corp is evidence, not proof. So the
    /// candidates are searched here - with `search_entities`, the same FTS lookup `resolve`
    /// prunes identity candidates with - and offered; whether any of them is what the
    /// value means is decided in the conversation, alongside the class decision that
    /// depends on the same reading of the entity.
    ///
    /// A value naming nothing simply gets no candidates. That is not a failure: the entity
    /// may not exist *yet*, and the attribute stays a literal until a later pass, when
    /// this runs again and finds it.
    async fn classification_evidence(
        &self,
        ontology_manager: &OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        links: &[KnowledgeEntityLink],
        progress: &mut ConsolidationProgress<'_>,
    ) -> (String, serde_json::Value) {
        let attrs = entity.attributes.as_object();
        let mut out = String::new();
        let mut entity_searches = Vec::new();
        let mut vocabulary_terms = BTreeSet::new();
        for link in links.iter().filter(|l| l.origin != LinkOrigin::Inferred) {
            if !link.relation.trim().is_empty() {
                vocabulary_terms.insert(link.relation.trim().to_string());
            }
        }
        if let Some(attrs) = attrs {
            for (key, value) in attrs {
                vocabulary_terms.insert(key.clone());
                out.push_str(&format!("- \"{key}\": {}\n", render_value(value)));
                // Search per element and bank the rendered result. A revision-scoped cache
                // never hides an entity minted or merged by an earlier loop iteration.
                for name in search_terms(value) {
                    let result = if let Some(cached) = progress.entity_search(&name) {
                        cached.to_string()
                    } else {
                        let hits = self
                            .ctx
                            .view
                            .search_entities(&name)
                            .await
                            .unwrap_or_default();
                        let candidates: Vec<String> = hits
                            .iter()
                            .filter(|h| h.path != entity.path)
                            .take(ATTRIBUTE_CANDIDATES)
                            .map(|h| format!("{} ({})", h.path, h.name))
                            .collect();
                        let rendered = if candidates.is_empty() {
                            "(no matching entity)".to_string()
                        } else {
                            candidates.join(", ")
                        };
                        progress.bank_entity_search(&name, rendered.clone());
                        rendered
                    };
                    if result != "(no matching entity)" {
                        out.push_str(&format!("    \"{name}\" may name: {result}\n"));
                    }
                    entity_searches.push(serde_json::json!({
                        "query": name,
                        "result": result,
                    }));
                }
            }
        }

        let mut vocabulary = Vec::new();
        for term in vocabulary_terms {
            let result = if let Some(cached) = progress.vocabulary_search(&term) {
                cached.to_string()
            } else {
                let hits = ontology_manager.search_vocab(&term, EVIDENCE_VOCAB_HITS);
                let rendered = if hits.is_empty() {
                    "(no matching standard term)".to_string()
                } else {
                    hits.iter()
                        .map(|hit| match &hit.label {
                            Some(label) => format!("{} [{}] — {label}", hit.curie, hit.kind),
                            None => format!("{} [{}]", hit.curie, hit.kind),
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                progress.bank_vocabulary_search(&term, rendered.clone());
                rendered
            };
            vocabulary.push(serde_json::json!({
                "query": term,
                "result": result,
            }));
        }

        (
            out,
            serde_json::json!({
                "status": "completed investigation; candidates are advisory",
                "vocabulary_searches": vocabulary,
                "entity_searches": entity_searches,
                "instruction": "Submit directly when this evidence is sufficient. Use tools only when more evidence could materially change the decision.",
            }),
        )
    }

    /// The "Map" half of classify: turn the entity's stated relations into *proposed* object
    /// properties plus the re-keys they imply. Pure - it commits no schema and touches
    /// no link. A `frona:` target gets a declaration (and its inverse, if named); a
    /// standard term is re-keyed without a redundant declaration. The satisfiability
    /// gate runs once over the whole proposal set during Assemble.
    pub(super) async fn relation_proposals(
        &self,
        ontology_manager: &OntologyManager,
        mappings: &[RelationMapping],
    ) -> (Vec<SchemaEdit>, Vec<(String, String)>) {
        let user_id = &self.ctx.scope.user_id;
        let Ok(effective_ontology) = ontology_manager.user_effective_ontology(user_id).await else {
            return (Vec::new(), Vec::new());
        };
        let px = effective_ontology.prefixes();
        let (mut edits, mut rekeys) = (Vec::new(), Vec::new());
        for m in mappings {
            let (from, to) = (m.from.trim(), m.to.trim());
            if from.is_empty() || to.is_empty() || from == to {
                continue;
            }
            if px.expand(to).starts_with("urn:frona:") {
                edits.push(SchemaEdit::DeclareObjectProperty {
                    property: to.to_string(),
                });
            }
            rekeys.push((from.to_string(), to.to_string()));
        }
        (edits, rekeys)
    }

    /// Turn one accepted classification into proposals - for the entity itself and for any
    /// entity it minted.
    ///
    /// **Both** the fresh conversation and the resume path that replays a banked
    /// classification go through here. That is not tidiness: minting writes entities and
    /// links, so a replay that skipped it would silently drop the entities the original
    /// attempt created, and the promotions naming them would then find no target and fall
    /// back to literals - the exact failure this whole change removes. Everything it does
    /// is idempotent, which is what lets the replay simply run it again.
    ///
    /// Returns `false` when the classification names no class, which is not something to
    /// record.
    pub(crate) async fn record_entity_proposal(
        &self,
        ontology_manager: &OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        c: &Classification,
        proposals: &mut ProposalSet,
        minted: &mut Vec<String>,
    ) -> bool {
        let classes = c.class_curies();
        if classes.is_empty() {
            return false;
        }
        let mut edits = classification_edits(c);
        // Mints first: the attribute half needs their paths to accept the targets naming
        // them, and an entity minted here is findable by the entities classified after it.
        let fresh = self.mint_entities(entity, &c.new_entities, proposals).await;
        minted.extend(fresh.iter().cloned());
        let staged_targets: Vec<String> = proposals.staged_entities.keys().cloned().collect();
        let (rel_edits, rekeys) = self
            .relation_proposals(ontology_manager, &c.relations)
            .await;
        edits.extend(rel_edits);
        let (attr_edits, attr_rekeys, promoted) = self
            .attribute_proposals(
                ontology_manager,
                entity,
                &c.attributes,
                &fresh,
                &staged_targets,
            )
            .await;
        tracing::debug!(
            entity = %entity.path,
            submitted_attributes = ?c.attributes,
            submitted_new_entities = ?c.new_entities,
            accepted_mints = ?fresh,
            literal_rekeys = ?attr_rekeys,
            promoted_edges = ?promoted,
            "pkm classify: attribute decisions"
        );
        edits.extend(attr_edits);
        proposals.record(
            &entity.path,
            EntityProposal {
                classes,
                edits,
                rekeys,
                attr_rekeys,
                promoted,
                promoted_sources: HashMap::new(),
                retracted: Vec::new(),
                has_keys: c.has_keys.clone(),
                inverse_functional_properties: c.inverse_functional_properties.clone(),
            },
        );
        proposals.record_declarations(&c.declarations);
        if !c.entity.name.trim().is_empty() {
            proposals
                .entity_shapes
                .insert(entity.path.clone(), c.entity.clone());
        }
        true
    }

    /// Create the entities the classification minted, hand each the facts that speak about
    /// it, and propose its class. Returns the paths an edge may now point at.
    ///
    /// Entities are created with **no kinds**. The class rides in [`ProposalSet`] and is
    /// stamped by the same commit that declares it, so the invariant holds unchanged: a
    /// entity is never typed with a term the TBox has not seen.
    ///
    /// Every step tolerates having been done before, because the resume path replays it.
    /// Failures warn rather than propagate - a mint that does not land costs an edge this
    /// pass, and the entity stays dirty for the next one.
    async fn mint_entities(
        &self,
        entity: &KnowledgeConsolidationEntity,
        mints: &[NewEntity],
        proposals: &mut ProposalSet,
    ) -> Vec<String> {
        if mints.is_empty() {
            return Vec::new();
        }
        let user_id = &self.ctx.scope.user_id;
        let px = self.prefixes.clone();
        // Cited fact IDs are checked against what the model was actually shown - the
        // entity's own current memories. A citation it invented links a real fact to the
        // wrong entity, which reads exactly like a true one afterwards.
        let mut shown: HashSet<String> = self
            .ctx
            .repo
            .current_memories_for_entity(user_id, &entity.path)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect();
        shown.extend(entity.source_memory_ids.iter().cloned());

        let mut out = Vec::new();
        for mint in accept_mints(mints, &entity.path, &px) {
            if let Some(existing) = proposals.input_entity(&mint.path) {
                if existing.category == EntityCategory::Concept {
                    out.push(mint.path);
                } else {
                    warn!(entity = %entity.path, minted = %mint.path,
                        "pkm classify: mint refused, staged path is not a concept entity");
                }
                continue;
            }
            match self.ctx.view.entity_by_path(&mint.path).await {
                // Already an entity - the model should have used `targets`, but the intent is
                // unambiguous, so take it as a target rather than costing a turn to say so.
                // A non-Concept entity is refused outright: an edge into a playbook or a note
                // mirror is not the relation it claims to be.
                Ok(Some(existing)) => {
                    if existing.category == EntityCategory::Concept {
                        out.push(mint.path);
                    } else {
                        warn!(
                            entity = %entity.path, minted = %mint.path,
                            "pkm classify: mint refused, path is not a concept entity"
                        );
                    }
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, minted = %mint.path, "pkm classify: mint lookup failed");
                    continue;
                }
            }
            let aliases = HashSet::new();
            let (search_names, search_name_tokens, search_assertions) =
                crate::memory::pkm::model::derive_resolution_search(
                    &mint.name,
                    &aliases,
                    &serde_json::json!({}),
                    std::iter::empty(),
                );
            let committed_shape = KnowledgeEntity {
                id: String::new(),
                user_id: user_id.to_string(),
                path: mint.path.clone(),
                origin: crate::memory::pkm::model::EntityOrigin::Internal,
                category: EntityCategory::Concept,
                kinds: Vec::new(),
                name: mint.name.clone(),
                description: mint.description.clone(),
                identity_evidence: Vec::new(),
                attribute_sources: Vec::new(),
                source_memory_ids: mint
                    .from_facts
                    .iter()
                    .filter(|id| shown.contains(*id))
                    .cloned()
                    .collect(),
                body: String::new(),
                sync_content: None,
                mirrored_rev: None,
                extracted_rev: None,
                related_playbooks: Vec::new(),
                search_text: crate::memory::pkm::model::derive_search_text(
                    &mint.name,
                    &mint.description,
                    &aliases,
                ),
                search_names,
                search_name_tokens,
                search_assertions,
                attributes: serde_json::json!({}),
                use_count: 0,
                aliases,
                rev: None,
                updated_at: chrono::Utc::now(),
                rendered_at: chrono::DateTime::<chrono::Utc>::MIN_UTC,
            };
            let mut staged = KnowledgeConsolidationEntity::from_committed(
                self.ctx.view.consolidation_id(),
                committed_shape,
            );
            staged.entity_id = None;
            proposals.stage_entity(staged);
            tracing::info!(
                entity = %entity.path, minted = %mint.path, facts = mint.from_facts.len(),
                "pkm classify: minted the entity an attribute value named"
            );
            proposals.record(
                &mint.path,
                EntityProposal {
                    classes: mint.classes,
                    edits: mint.edits,
                    rekeys: Vec::new(),
                    attr_rekeys: Vec::new(),
                    promoted: Vec::new(),
                    promoted_sources: HashMap::new(),
                    retracted: Vec::new(),
                    has_keys: Vec::new(),
                    inverse_functional_properties: Vec::new(),
                },
            );
            out.push(mint.path);
        }
        out
    }

    /// The attribute half: turn each mapping into a *proposed* property declaration plus
    /// either a re-key (it stays a literal) or a promotion (it becomes an edge).
    ///
    /// The declaration kind follows the decision, which is the point of the stage: a
    /// promoted attribute is declared an **object** property, one that stays is declared a
    /// **data** property. Nothing is committed here - the whole proposal set goes through
    /// adjudicate's gate during Assemble, so a term promoted on the strength of one entity is
    /// still judged against every entity that uses it.
    ///
    /// `minted` are the paths this entity's mints just created - targets that are legitimate
    /// even though they did not exist when the conversation started.
    ///
    /// Returns `(edits, attr_rekeys, promoted)`.
    pub(super) async fn attribute_proposals(
        &self,
        ontology_manager: &OntologyManager,
        entity: &KnowledgeConsolidationEntity,
        mappings: &[AttributeMapping],
        minted: &[String],
        staged: &[String],
    ) -> AttributeDecisions {
        let user_id = &self.ctx.scope.user_id;
        let Ok(effective_ontology) = ontology_manager.user_effective_ontology(user_id).await else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let held: HashSet<&str> = entity
            .attributes
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.as_str())
            .collect();
        // Which targets are real. `knowledge_entity_link` stores `to_entity_path` as a plain string
        // and the commit never checks it, so a path the model invented would be written as
        // an edge to nothing and nothing downstream would notice. The prompt asks for a
        // path that was offered or found; this is what holds it to that.
        // A target may already be committed, minted by this response, or waiting in the
        // same pass's virtual entity set. Looking only in the live repository made a model
        // that correctly reused `organizations/example-corp` lose the edge, while a model that
        // redundantly re-minted Example Corp happened to pass.
        let mut known: HashSet<String> = minted.iter().chain(staged).cloned().collect();
        for t in mappings.iter().flat_map(|m| &m.targets).map(|t| t.trim()) {
            if t.is_empty() || known.contains(t) {
                continue;
            }
            if matches!(self.ctx.view.entity_by_path(t).await, Ok(Some(_))) {
                known.insert(t.to_string());
            }
        }
        attribute_edits(
            mappings,
            &entity.path,
            &held,
            &known,
            effective_ontology.prefixes(),
        )
    }
}
