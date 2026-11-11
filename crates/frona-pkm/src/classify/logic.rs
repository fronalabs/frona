use super::{ClassifyInput, ClassifyPrompts, claim_automatic_identity_discovery, proposal::*};
use crate::prompt::PromptIds;
use crate::{
    Error,
    candidates::{RESOLUTION_PROMPT_LIMIT, Request, Search, Subject},
    model::{
        EntityCategory, KnowledgeConsolidationEntity, KnowledgeEntity, KnowledgeEntityLink,
        LinkOrigin,
    },
    ontology::{Ontology, PrefixMap, SchemaEdit},
    projection::{projection_rejection_details, validate_proposal_projection},
};
use frona_harness::Decision;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tracing::warn;
#[derive(Clone)]
pub(super) struct ClassifyLogic {
    pub retrieval: Option<Arc<dyn crate::candidates::CandidateRetrieval>>,
    pub input: Arc<ClassifyInput>,
    pub prefixes: PrefixMap,
    pub prompts: ClassifyPrompts,
}
/// Application-owned validation state. The inspector and research tools share the
/// same diagnostics, while each async inspection owns its state at Send boundaries.
pub(super) struct ClassificationInspection {
    pub logic: ClassifyLogic,
    pub ontology: Ontology,
    pub entity: KnowledgeConsolidationEntity,
    pub proposals: ProposalSet,
    pub prompt_ids: PromptIds,
    pub existing: HashSet<String>,
    pub overlay: Arc<crate::tools::ontology::OntologyToolOverlay>,
    pub requires_page_shape: bool,
    pub attempts: AtomicUsize,
    pub failures: AtomicUsize,
    pub details: std::sync::Mutex<Vec<String>>,
    pub challenged: AtomicBool,
}

impl ClassificationInspection {
    pub async fn inspect(
        &self,
        candidate: Classification,
    ) -> Result<Decision<Classification>, Error> {
        let ctx = &self.logic.input;
        let prefixes = &self.logic.prefixes;
        let ontology_manager = &self.ontology;
        let entity = &self.entity;
        let identity_page = entity;
        let proposals_snapshot = &self.proposals;
        let validation_prompt_ids = &self.prompt_ids;
        let existing = &self.existing;
        let diagnostic_store = &self.overlay.diagnostics;
        let discovery_calls = self.overlay.allowance.attempts();
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
            let candidates = Search::new(ctx.graph.clone())
                .with_retrieval(self.logic.retrieval.clone())
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
        if let Some(feedback) = self.logic.prompts.bad_term(prefixes, candidate.terms())? {
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
            .logic
            .mint_entities(entity, &validation_candidate.new_entities, &mut trial)
            .await;
        let staged_targets: Vec<String> = trial.staged_entities.keys().cloned().collect();
        let mut edits = classification_edits(&validation_candidate);
        let (relation_edits, rekeys) = self
            .logic
            .relation_proposals(ontology_manager, &validation_candidate.relations)
            .await;
        let (attribute_edits, attr_rekeys, promoted) = self
            .logic
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
        let validation = validate_proposal_projection(&ctx.graph, ontology_manager, &trial).await?;
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
        let feedback = self.logic.prompts.reject(&[
            ("violations", detail.as_str()),
            ("class", classes.join(", ").as_str()),
        ])?;
        Ok(Decision::revise(feedback))
    }
}

impl ClassifyLogic {
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
    pub(super) async fn classification_evidence(
        &self,
        ontology_manager: &Ontology,
        entity: &KnowledgeConsolidationEntity,
        links: &[KnowledgeEntityLink],
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
                // Search each value against this invocation's snapshot. The host supplies
                // a refreshed snapshot after applying a previous phase output.
                for name in search_terms(value) {
                    let hits = self.input.graph.search_entities(&name);
                    let candidates: Vec<_> = hits
                        .iter()
                        .filter(|h| h.path != entity.path)
                        .take(ATTRIBUTE_CANDIDATES)
                        .map(|h| format!("{} ({})", h.path, h.name))
                        .collect();
                    let result = if candidates.is_empty() {
                        "(no matching entity)".into()
                    } else {
                        candidates.join(", ")
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
            let hits = ontology_manager.search_vocab(&term, EVIDENCE_VOCAB_HITS);
            let result = if hits.is_empty() {
                "(no matching standard term)".into()
            } else {
                hits.iter()
                    .map(|hit| match &hit.label {
                        Some(label) => format!("{} [{}] — {label}", hit.curie, hit.kind),
                        None => format!("{} [{}]", hit.curie, hit.kind),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
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
        ontology_manager: &Ontology,
        mappings: &[RelationMapping],
    ) -> (Vec<SchemaEdit>, Vec<(String, String)>) {
        let user_id = &self.input.graph.scope_id;
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
    /// Mints stay virtual in the returned proposal set. Relation and attribute
    /// compilation can therefore refer to them before any host persistence.
    ///
    /// Returns `false` when the classification names no class, which is not something to
    /// record.
    pub(crate) async fn record_entity_proposal(
        &self,
        ontology_manager: &Ontology,
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
    /// Existing concept paths are reusable targets. Invalid or unavailable targets
    /// are omitted; the host decides whether to retry them with a refreshed snapshot.
    async fn mint_entities(
        &self,
        entity: &KnowledgeConsolidationEntity,
        mints: &[NewEntity],
        proposals: &mut ProposalSet,
    ) -> Vec<String> {
        if mints.is_empty() {
            return Vec::new();
        }
        let user_id = &self.input.graph.scope_id;
        let px = self.prefixes.clone();
        // Cited fact IDs are checked against what the model was actually shown - the
        // entity's own current memories. A citation it invented links a real fact to the
        // wrong entity, which reads exactly like a true one afterwards.
        let mut shown: HashSet<String> = self
            .input
            .graph
            .current_memories_for_entity(&entity.path)
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
            match self.input.graph.entity_by_path(&mint.path) {
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
                crate::model::derive_resolution_search(
                    &mint.name,
                    &aliases,
                    &serde_json::json!({}),
                    std::iter::empty(),
                );
            let committed_shape = KnowledgeEntity {
                id: String::new(),
                user_id: user_id.to_string(),
                path: mint.path.clone(),
                origin: crate::model::EntityOrigin::Internal,
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
                search_text: crate::model::derive_search_text(
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
                &entity.consolidation_id,
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
        ontology_manager: &Ontology,
        entity: &KnowledgeConsolidationEntity,
        mappings: &[AttributeMapping],
        minted: &[String],
        staged: &[String],
    ) -> AttributeDecisions {
        let user_id = &self.input.graph.scope_id;
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
            if matches!(self.input.graph.entity_by_path(t), Ok(Some(_))) {
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
