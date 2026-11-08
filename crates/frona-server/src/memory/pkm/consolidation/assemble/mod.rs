//! **Assemble** - adjudicate proposed schema changes, then commit them with entity types.
//!
//! The pass's undeclared terms are adjudicated as a batch, each decision dry-run through
//! the pure guardrail in [`adjudicate`],
//! and whatever clears is written together with the entity types stamped against it - one
//! CAS transaction, because an entity typed with a term the TBox never declared is an entity
//! the reasoner cannot place.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use oxrdf::Triple;
use tracing::warn;

pub(super) mod adjudicate;
mod inspection;
use inspection::{AdjudicationCandidate, AdjudicationInspection};

use crate::{core::error::AppError, tool::registry::ToolFilter};
use crate::{
    db::repo::pkm::AttributeOps,
    memory::pkm::{
        consolidation::{AssembleOutcome, PromptSpec},
        model::{KnowledgeConsolidationEntity, KnowledgeEntity},
        ontology::{
            AlignKind, Characteristic, OntologyManager, OverrideTarget, SchemaEdit, TypePlan,
        },
    },
};
use adjudicate::{
    AdjudicationResult, Decision, GateOutcome, Proposal, ProposalKind, gate, partition_proposals,
};
use frona_harness::{FailureKind, Incomplete, RunOutcome};

use super::{ConsolidationProgress, Consolidator, bad_term_feedback};
use crate::memory::pkm::consolidation::classify::ProposalSet;

fn schema_edit_mentions(edit: &SchemaEdit, term: &str) -> bool {
    match edit {
        SchemaEdit::AnnotateComment {
            term: annotated, ..
        } => annotated == term,
        SchemaEdit::DeclareClass { class } => class == term,
        SchemaEdit::SubClassOf { sub, .. } => sub == term,
        SchemaEdit::EquivalentClasses { a, .. }
        | SchemaEdit::DisjointClasses { a, .. }
        | SchemaEdit::EquivalentProperties { a, .. }
        | SchemaEdit::InverseProperties { a, .. } => a == term,
        SchemaEdit::DeclareObjectProperty { property }
        | SchemaEdit::DeclareDataProperty { property }
        | SchemaEdit::PropertyCharacteristic { property, .. }
        | SchemaEdit::ObjectPropertyDomain { property, .. }
        | SchemaEdit::ObjectPropertyRange { property, .. }
        | SchemaEdit::RestrictDatatype { property, .. } => property == term,
        SchemaEdit::SubPropertyOf { sub, .. } => sub == term,
        SchemaEdit::Align { frona, .. } => frona == term,
        SchemaEdit::AmendOverride { .. } => false,
    }
}

/// Re-plan attempts when a concurrent writer moves the schema delta under the commit.
const SCHEMA_COMMIT_ATTEMPTS: usize = 3;

struct AdjudicationInput<'a> {
    candidates: &'a [Proposal],
    projected_abox: &'a [Triple],
    accepted_edits: &'a [SchemaEdit],
    initial_rejection: Option<&'a str>,
}

/// One retractable axiom, in the shape the model has to name it back as. Rendered to
/// mirror the `amend` submit fields exactly - an axiom it can read but cannot address is
/// worse than not showing it, because the rejection feedback then looks unactionable.
fn render_override_target(t: &OverrideTarget) -> String {
    match t {
        OverrideTarget::Disjoint { a, b } => {
            format!("disjoint: {a} ⊥ {b}   (kind=\"disjoint\", a=\"{a}\", b=\"{b}\")")
        }
        OverrideTarget::Facet { property } => {
            format!("facet on {property}   (kind=\"facet\", property=\"{property}\")")
        }
        OverrideTarget::Characteristic {
            property,
            characteristic,
        } => {
            let name = match characteristic {
                Characteristic::Functional => "functional",
                Characteristic::Transitive => "transitive",
                Characteristic::Symmetric => "symmetric",
                Characteristic::Asymmetric => "asymmetric",
                Characteristic::Irreflexive => "irreflexive",
            };
            format!(
                "{property} is {name}   (kind=\"characteristic\", property=\"{property}\", \
                 characteristic=\"{name}\")"
            )
        }
    }
}

/// What adjudicate settled on: the schema to commit once, the term renames an `align` or
/// `merge` implies, and the isolated fallout the ladder admitted.
#[derive(Debug, Default)]
struct AssemblePlan {
    edits: Vec<SchemaEdit>,
    /// proposed term → the term entities should actually be stamped with.
    renames: HashMap<String, String>,
    /// Entity paths whose live facts an adopted edit breaks.
    quarantine: Vec<String>,
    declared: usize,
    aligned: usize,
    deferred: usize,
    nominations: Vec<Proposal>,
}

impl AssemblePlan {
    fn merge(&mut self, other: AssemblePlan) {
        self.edits = other.edits;
        self.renames.extend(other.renames);
        self.quarantine.extend(other.quarantine);
        self.quarantine.sort();
        self.quarantine.dedup();
        self.declared += other.declared;
        self.aligned += other.aligned;
        self.deferred += other.deferred;
        for nomination in other.nominations {
            if !self.nominations.iter().any(|held| {
                held.term == nomination.term && held.proposed_edits == nomination.proposed_edits
            }) {
                self.nominations.push(nomination);
            }
        }
    }

    fn note(&mut self, term: &str, decision: &Decision) {
        if let Some(target) = decision.rename_target() {
            self.renames.insert(term.to_string(), target.to_string());
            self.aligned += 1;
        } else if decision.label() == "defer" {
            self.deferred += 1;
        } else {
            self.declared += 1;
        }
    }

    /// The term an entity is stamped with: the alignment/merge target if the pass
    /// redirected it, else the term classify proposed.
    fn final_term<'a>(&'a self, proposed: &'a str) -> &'a str {
        self.renames
            .get(proposed)
            .map(String::as_str)
            .unwrap_or(proposed)
    }
}

/// Explicit application policy for partial adjudication. Cancellation and required
/// storage failure stop before banking. Other interrupted work may be salvaged only
/// when inspection actually retained a candidate, and the caller re-gates it afresh.
fn retained_for_salvage(
    incomplete: Incomplete<AdjudicationCandidate>,
) -> Result<AdjudicationCandidate, AppError> {
    warn!(reason = %incomplete.reason, outcome = ?incomplete.report.outcome,
        retained_edits = incomplete.retained.as_ref().map(|candidate| candidate.plan.edits.len()),
        "pkm assemble: adjudication incomplete; evaluating application salvage");
    match incomplete.report.outcome {
        Some(RunOutcome::Incomplete {
            kind: FailureKind::Cancelled,
            ..
        }) => {
            return Err(AppError::Inference(Box::new(
                frona_harness::inference::InferenceError::Cancelled(incomplete.reason.to_string()),
            )));
        }
        Some(RunOutcome::Incomplete {
            kind: FailureKind::Storage,
            ..
        }) => {
            return Err(AppError::Internal(format!(
                "adjudication storage failed: {}",
                incomplete.reason
            )));
        }
        _ => {}
    }
    incomplete.retained.ok_or_else(|| {
        AppError::Internal(format!(
            "adjudication ended without retained work: {}",
            incomplete.reason,
        ))
    })
}

impl Consolidator {
    /// Finalise the pass. The pass's undeclared
    /// terms are put to the model as a batch; it decides each one (declare / align /
    /// merge / restrict / defer) while code enforces the guardrails; then the accepted
    /// schema is committed **once** and each surviving entity is stamped **once** with
    /// its *final* term - which is not necessarily the term classify proposed, since an
    /// `align` or `merge` renames it. Returns how many entities were stamped.
    ///
    /// Batching is the point: seeing the whole pass at once is what lets the model
    /// collapse two proposals that mean the same thing, and deciding before any commit
    /// is what lets it write `schema:Bar` directly instead of minting `frona:Foo` and
    /// realigning it later.
    pub(super) async fn assemble(
        &self,
        ontology_manager: &OntologyManager,
        entities: &[KnowledgeConsolidationEntity],
        proposals: &ProposalSet,
        stats: &mut AssembleOutcome,
        quarantined: &mut HashSet<String>,
        progress: &mut ConsolidationProgress<'_>,
    ) -> Result<usize, AppError> {
        if proposals.by_path.is_empty() {
            return Ok(0);
        }

        let projected_abox = self
            .projected_abox(ontology_manager, proposals, Some(entities))
            .await?;
        // Reconcile changes the proposal overlay, not the original `entities` snapshot.
        // Scan the entity view so a property first introduced by Reconcile is
        // adjudicated before the final A-box/T-box invariant runs.
        let projected_pages = proposals.reasoning_entities(
            entities
                .iter()
                .map(KnowledgeConsolidationEntity::as_knowledge_entity)
                .collect(),
        );
        let candidates = self
            .undeclared_terms(ontology_manager, &projected_pages, proposals)
            .await;
        let outcome = if candidates.is_empty() {
            AssemblePlan::default()
        } else {
            let mut all = AssemblePlan::default();
            let mut pending = candidates.clone();
            let mut replay_rejection = None;
            if let Some(banked) = progress.adjudication() {
                for nomination in &banked.amendment_nominations {
                    let proposal = Proposal {
                        term: nomination.term.trim().to_string(),
                        kind: nomination.term_kind,
                        usage_entities: 0,
                        usage_links: 0,
                        description: "Existing axiom nominated for repair.".into(),
                        proposed_edits: vec![SchemaEdit::AmendOverride {
                            target: nomination.target.clone(),
                        }],
                    };
                    if !pending.iter().any(|held| {
                        held.term == proposal.term && held.proposed_edits == proposal.proposed_edits
                    }) {
                        pending.push(proposal);
                    }
                }
                let decided_terms: HashSet<_> = banked
                    .decisions
                    .iter()
                    .map(|decision| decision.term.trim().to_string())
                    .collect();
                let decided: Vec<_> = pending
                    .iter()
                    .filter(|candidate| decided_terms.contains(candidate.term.trim()))
                    .cloned()
                    .collect();
                if !decided.is_empty() {
                    let (replayed, rejected) = self
                        .apply_adjudication(ontology_manager, &decided, banked, &projected_abox)
                        .await;
                    if rejected.is_empty() {
                        all.merge(replayed);
                        pending.retain(|candidate| !decided_terms.contains(candidate.term.trim()));
                    } else {
                        replay_rejection = Some(rejected.join("\n"));
                    }
                }
            }
            while pending.len() >= adjudicate::ADJUDICATION_BATCH_MIN {
                let partition = partition_proposals(&pending);
                let Some(batch) = partition.batches.into_iter().next() else {
                    pending = partition.final_tail;
                    break;
                };
                let completed: HashSet<String> = batch
                    .iter()
                    .map(|candidate| candidate.term.clone())
                    .collect();
                let mut nominations = Vec::new();
                let initial_rejection = replay_rejection.take();
                match self
                    .adjudicate_schema(
                        ontology_manager,
                        proposals,
                        progress,
                        AdjudicationInput {
                            candidates: &batch,
                            projected_abox: &projected_abox,
                            accepted_edits: &all.edits,
                            initial_rejection: initial_rejection.as_deref(),
                        },
                    )
                    .await
                {
                    Ok(mut outcome) => {
                        nominations.append(&mut outcome.nominations);
                        all.merge(outcome);
                    }
                    Err(error) => return Err(error),
                }
                pending.retain(|candidate| !completed.contains(&candidate.term));
                for nomination in nominations {
                    if !pending.iter().any(|held| {
                        held.term == nomination.term
                            && held.proposed_edits == nomination.proposed_edits
                    }) {
                        pending.push(nomination);
                    }
                }
                // Rebuild pending declarations from the state produced by the previous
                // patch before choosing the next hierarchy partition.
                for candidate in &mut pending {
                    let refreshed: Vec<_> = all
                        .edits
                        .iter()
                        .filter(|edit| schema_edit_mentions(edit, &candidate.term))
                        .cloned()
                        .collect();
                    if !refreshed.is_empty() {
                        candidate.proposed_edits = refreshed;
                    }
                }
            }
            if replay_rejection.is_some() && !pending.is_empty() {
                let initial_rejection = replay_rejection.take();
                let outcome = self
                    .adjudicate_schema(
                        ontology_manager,
                        proposals,
                        progress,
                        AdjudicationInput {
                            candidates: &pending,
                            projected_abox: &projected_abox,
                            accepted_edits: &all.edits,
                            initial_rejection: initial_rejection.as_deref(),
                        },
                    )
                    .await?;
                all.merge(outcome);
                pending.clear();
            }
            let mut rejected_tail = Vec::new();
            for candidate in pending {
                let mut trial = all.edits.clone();
                for edit in &candidate.proposed_edits {
                    if !trial.contains(edit) {
                        trial.push(edit.clone());
                    }
                }
                let impact = ontology_manager
                    .test_edits_with_abox(
                        &self.ctx.scope.user_id,
                        &trial,
                        &validation_abox(&projected_abox, &trial, &self.prefixes),
                    )
                    .await?;
                if !impact.incoherence.is_empty() || !impact.data_violations.is_empty() {
                    rejected_tail.push(candidate);
                    continue;
                }
                all.edits = trial;
                all.note(&candidate.term, &Decision::AcceptProposal);
            }
            if !rejected_tail.is_empty() {
                let outcome = self
                    .adjudicate_schema(
                        ontology_manager,
                        proposals,
                        progress,
                        AdjudicationInput {
                            candidates: &rejected_tail,
                            projected_abox: &projected_abox,
                            accepted_edits: &all.edits,
                            initial_rejection: replay_rejection.as_deref(),
                        },
                    )
                    .await?;
                all.merge(outcome);
            }
            all
        };

        if !outcome.quarantine.is_empty() {
            return Err(AppError::Internal(format!(
                "assemble invariant: accepted schema projection would invalidate entities: {:?}",
                outcome.quarantine,
            )));
        }

        // Defensive invariant only. Model-originated failures must already have been
        // returned by Classify, Resolve, Reconcile, or Adjudicate before acceptance.
        let final_impact = ontology_manager
            .test_edits_with_abox(
                &self.ctx.scope.user_id,
                &outcome.edits,
                &validation_abox(&projected_abox, &outcome.edits, &self.prefixes),
            )
            .await?;
        if !final_impact.incoherence.is_empty() || !final_impact.data_violations.is_empty() {
            return Err(AppError::Internal(format!(
                "assemble invariant: a stage accepted an invalid graph patch: {}",
                format_edit_impact(&final_impact),
            )));
        }

        // Decide everything, then write it once. The schema delta and the entity types
        // stamped against it have to land together: an entity typed with a term the TBox
        // never declared is an entity the reasoner cannot place, and that is exactly what
        // this stage produced whenever adjudication failed half-way through.
        let stamped = self
            .commit(ontology_manager, entities, proposals, &outcome, progress)
            .await?;

        tracing::info!(
            declared = outcome.declared,
            aligned = outcome.aligned,
            deferred = outcome.deferred,
            committed_edits = outcome.edits.len(),
            quarantined_pages = outcome.quarantine.len(),
            "pkm assemble: schema adjudicated"
        );

        let _ = (stats, quarantined);
        Ok(stamped)
    }

    /// The complete ABox the single commit will produce, assembled without writing.
    pub(super) async fn projected_abox(
        &self,
        ontology_manager: &OntologyManager,
        proposals: &ProposalSet,
        seed_entities: Option<&[KnowledgeConsolidationEntity]>,
    ) -> Result<Vec<Triple>, AppError> {
        let user_id = &self.ctx.scope.user_id;
        let entities = match seed_entities {
            Some(entities) => entities
                .iter()
                .map(KnowledgeConsolidationEntity::as_knowledge_entity)
                .collect(),
            None => self
                .ctx
                .view
                .list_entities()
                .await?
                .into_iter()
                .map(|entity| entity.as_knowledge_entity())
                .collect(),
        };
        let links = self.ctx.repo.asserted_links(user_id).await?;
        let (entities, links) = proposals.project_graph(user_id, entities, links);
        Ok(ontology_manager.assertion_graph(&entities, &links))
    }

    /// The terms this pass used that the TBox does not declare yet - the batch
    /// adjudicate rules on, each with its **global** usage so the model can tell a
    /// load-bearing term from a one-off. Standard-vocabulary terms are already
    /// declared by the bundled ontologies and are never proposals.
    async fn undeclared_terms(
        &self,
        ontology_manager: &OntologyManager,
        entities: &[KnowledgeEntity],
        proposals: &ProposalSet,
    ) -> Vec<Proposal> {
        let user_id = &self.ctx.scope.user_id;
        let Ok(effective_ontology) = ontology_manager.user_effective_ontology(user_id).await else {
            return Vec::new();
        };
        let px = effective_ontology.prefixes();
        let declared: HashSet<String> = match ontology_manager.catalog(user_id).await {
            Ok(c) => c
                .classes
                .into_iter()
                .chain(c.object_properties)
                .chain(c.data_properties)
                .collect(),
            Err(_) => HashSet::new(),
        };
        let minted = |t: &str| px.expand(t).starts_with("urn:frona:") && !declared.contains(t);

        // BTreeMap: dedupe across entities, and give the model a stable order.
        let mut terms: BTreeMap<String, ProposalKind> = BTreeMap::new();
        // Every term is **repaired before it becomes a proposal**, so the string the model
        // is shown, the string it answers with, and the string written to the schema are one
        // string. Listing a term as `forum_url` and requiring the answer verbatim asked the
        // model to echo a spelling it had every reason to correct - and it did correct it,
        // to `frona:forum_url`, which then matched nothing.
        //
        // Repairing before the dedupe also collapses spellings: `manufacturer`,
        // `frona:manufacturer` and `urn:frona:manufacturer` reach this map as one key
        // instead of three proposals for one concept.
        let mut propose = |raw: &str, kind: ProposalKind| {
            let raw = raw.trim();
            if raw.is_empty() {
                return;
            }
            match px.repair_term(raw, kind.term_kind()) {
                // `minted` is checked on the repaired term: whether it is already declared
                // is a question about the term, not about how this pass happened to spell it.
                Ok(t) => {
                    if minted(&t) {
                        terms.insert(t, kind);
                    }
                }
                Err(e) => warn!(
                    term = %raw,
                    reason = %e.reason,
                    "assemble: term cannot be repaired, not proposing it"
                ),
            }
        };
        for p in proposals.by_path.values() {
            for class in &p.classes {
                propose(class, ProposalKind::Class);
            }
            for (_, to) in &p.rekeys {
                propose(to, ProposalKind::ObjectProperty);
            }
            // The attribute decisions, each proposed as the kind it was decided to be.
            // Without these the terms classify mints for attributes reach adjudicate a
            // whole pass late - declared only once reconcile has written them onto an entity
            // - so a promoted property spends that pass with no domain, no range and no
            // inverse, and the reasoner has nothing to materialize from.
            for (_, to, _) in &p.promoted {
                propose(to, ProposalKind::ObjectProperty);
            }
            for (_, to) in &p.attr_rekeys {
                propose(to, ProposalKind::DataProperty);
            }
        }
        // Data properties also come from the entities' own CURIE-keyed attributes, which
        // covers keys no classification touched this pass (earlier passes, note-ingest).
        // An undeclared one is how a facet - `frona:port` in [1,65535] - gets proposed
        // and bounded.
        for entity in entities {
            let Some(attrs) = entity.attributes.as_object() else {
                continue;
            };
            let retired_keys = proposals
                .by_path
                .get(&entity.path)
                .map(|proposal| {
                    proposal
                        .promoted
                        .iter()
                        .map(|(key, _, _)| key.as_str())
                        .chain(proposal.attr_rekeys.iter().map(|(key, _)| key.as_str()))
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            for key in attrs.keys() {
                if retired_keys.contains(key.as_str()) {
                    continue;
                }
                propose(key, ProposalKind::DataProperty);
            }
        }

        let mut out = Vec::with_capacity(terms.len());
        for (term, kind) in terms {
            let (usage_entities, usage_links) = ontology_manager
                .usage_impact(user_id, &term)
                .await
                .unwrap_or((0, 0));
            let mut proposed_edits: Vec<_> = proposals
                .proposed_edits
                .iter()
                .filter(|edit| schema_edit_mentions(edit, &term))
                .cloned()
                .collect();
            if proposed_edits.is_empty() {
                proposed_edits.push(match kind {
                    ProposalKind::Class => SchemaEdit::DeclareClass {
                        class: term.clone(),
                    },
                    ProposalKind::ObjectProperty => SchemaEdit::DeclareObjectProperty {
                        property: term.clone(),
                    },
                    ProposalKind::DataProperty => SchemaEdit::DeclareDataProperty {
                        property: term.clone(),
                    },
                });
            }
            let model_description = proposals
                .declaration_descriptions
                .iter()
                .find(|(declared, _)| px.expand(declared) == px.expand(&term))
                .map(|(_, description)| description.clone());
            let description = model_description.clone().unwrap_or_else(||
                "No model-authored intent was preserved; keep only the validated baseline declaration.".into());
            if let Some(comment) = model_description.filter(|comment| !comment.trim().is_empty()) {
                proposed_edits.push(SchemaEdit::AnnotateComment {
                    term: term.clone(),
                    comment,
                });
            }
            out.push(Proposal {
                term,
                kind,
                usage_entities,
                usage_links,
                description,
                proposed_edits,
            });
        }
        out
    }

    /// The adjudicate conversation plus the guardrail loop. The model submits a decision
    /// per term; each is dry-run through [`gate`] against the real ABox, cumulatively
    /// with what has already been accepted - that is the edit set actually heading for
    /// the commit, so it is the honest thing to ask about. Rejected terms are fed back
    /// for revision up to the configured amend budget; whatever is accepted when the
    /// budget runs out still commits.
    async fn adjudicate_schema(
        &self,
        ontology_manager: &OntologyManager,
        proposals: &ProposalSet,
        progress: &mut ConsolidationProgress<'_>,
        input: AdjudicationInput<'_>,
    ) -> Result<AssemblePlan, AppError> {
        let AdjudicationInput {
            candidates,
            projected_abox,
            accepted_edits,
            initial_rejection,
        } = input;
        let user_id = &self.ctx.scope.user_id;
        let px = self.prefixes.clone();

        // A proposal whose own term cannot be written is dropped rather than asked about.
        // The model has to name a term verbatim to decide it, so it has no move that would
        // fix one - and asking would burn the whole revision budget on a term that can only
        // ever be rejected, taking the pass's other decisions down with it. These come from
        // stored usage, so one is data written before the terms were validated at all.
        let candidates: Vec<Proposal> = candidates
            .iter()
            .filter(|c| match px.validate_term(&c.term) {
                Ok(()) => true,
                Err(e) => {
                    warn!(term = %c.term, reason = %e.reason, "adjudicate: skipping unwritable term");
                    false
                }
            })
            .cloned()
            .collect();
        let candidates = &candidates[..];

        let durable = self
            .ctx
            .view
            .list_entities()
            .await?
            .into_iter()
            .map(|entity| entity.as_knowledge_entity())
            .collect();
        let entities = proposals.reasoning_entities(durable);

        let mut block = String::new();
        for c in candidates {
            block.push_str(&format!(
                "- {} ({}) — used by {} entity(s), {} link(s)\n  intent: {}\n",
                c.term,
                match c.kind {
                    ProposalKind::Class => "class",
                    ProposalKind::ObjectProperty => "object property",
                    ProposalKind::DataProperty => "data property",
                },
                c.usage_entities,
                c.usage_links,
                c.description,
            ));
            let expanded = px.expand(&c.term);
            let examples: Vec<String> = match c.kind {
                ProposalKind::Class => entities
                    .iter()
                    .filter(|entity| entity.kinds.iter().any(|kind| px.expand(kind) == expanded))
                    .take(5)
                    .map(|entity| {
                        format!(
                            "  entity={} name={:?} kinds={:?} attributes={}",
                            entity.path, entity.name, entity.kinds, entity.attributes,
                        )
                    })
                    .collect(),
                ProposalKind::DataProperty => entities
                    .iter()
                    .filter(|entity| {
                        entity
                            .attributes
                            .as_object()
                            .is_some_and(|attrs| attrs.keys().any(|key| px.expand(key) == expanded))
                    })
                    .take(5)
                    .map(|entity| {
                        format!(
                            "  entity={} name={:?} attributes={}",
                            entity.path, entity.name, entity.attributes,
                        )
                    })
                    .collect(),
                ProposalKind::ObjectProperty => projected_abox
                    .iter()
                    .filter(|triple| triple.predicate.as_str() == expanded)
                    .take(5)
                    .map(|triple| format!("  triple={triple}"))
                    .collect(),
            };
            if !examples.is_empty() {
                block.push_str(&examples.join("\n"));
                block.push('\n');
            }
        }
        // The axioms already in force that could be loosened. The proposal set above is
        // only the terms *this* pass used, so without this the model can see that an edit
        // was rejected but not the committed axiom doing the rejecting - which is what
        // The model must see committed constraints so it can amend the one that rejects an edit.
        let in_force = ontology_manager
            .retractable(user_id)
            .await
            .unwrap_or_default();
        let mut axioms = String::new();
        for t in &in_force {
            axioms.push_str(&format!("- {}\n", render_override_target(t)));
        }
        let vars: &[(&str, &str)] = &[
            ("proposals", &block),
            (
                "axioms",
                if axioms.is_empty() {
                    "(none)\n"
                } else {
                    &axioms
                },
            ),
        ];
        let overlay = std::sync::Arc::new(
            crate::memory::pkm::consolidation::tools::ontology::OntologyToolOverlay {
                entities,
                proposed_edits: proposals.proposed_edits.clone(),
                abox: projected_abox.to_vec(),
                diagnostics: Default::default(),
                prefixes: self.prefixes.clone(),
                tool_budget: candidates.len().saturating_mul(5),
                tool_calls: Default::default(),
            },
        );
        let tools =
            crate::memory::pkm::consolidation::tools::ontology::build_ontology_tools_with_overlay(
                ontology_manager.clone(),
                &self.ctx,
                self.prefixes.clone(),
                Some(overlay),
                crate::memory::pkm::consolidation::tools::ontology::OntologyToolProfile::Assemble,
            );
        let suffix = initial_rejection.map(|rejection| format!(
            "\n\nThe checkpointed adjudication is no longer valid against the reconstructed graph. Revise these rejected decisions:\n{rejection}"
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
        let run = harness
            .run_with_input_suffix(PromptSpec::ASSEMBLE, vars, &suffix)?
            .exploration_turns(candidates.len().saturating_mul(5).saturating_add(4))
            .structured::<AdjudicationResult>()
            .submission_attempts(self.config.pkm_adjudication_max_attempts_per_batch);
        let inspection = Arc::new(AdjudicationInspection::new(
            self.clone(),
            ontology_manager.clone(),
            candidates.to_vec(),
            projected_abox.to_vec(),
            accepted_edits.to_vec(),
        ));
        let refined = run
            .inspect(move |submission| {
                let inspection = inspection.clone();
                async move { inspection.inspect(submission).await }
            })
            .await;

        // SDK success and application salvage stay separate. The retained value
        // contains both the evaluated plan and its exact source submission; an
        // incomplete result never becomes accepted by selecting that value.
        let candidate = match refined {
            Ok(accepted) => accepted,
            Err(incomplete) => retained_for_salvage(incomplete)?,
        };
        let (best, rejected) = self
            .gate_submission(
                ontology_manager,
                candidates,
                &candidate.submission,
                projected_abox,
                accepted_edits,
            )
            .await?;
        if !rejected.is_empty() {
            tracing::info!(rejections = ?rejected, retained_edits = candidate.plan.edits.len(),
                salvaged_edits = best.edits.len(), "adjudicate: re-gated application subset");
        }
        let best = self
            .validate(ontology_manager, best, projected_abox)
            .await?;
        // Bank only after independent re-gating and cumulative validation. Replay
        // banks the source decision, never an old claim that its edits remain valid.
        progress.bank_adjudication(&candidate.submission).await?;
        progress
            .checkpoint_transition("adjudicate", "schema", proposals)
            .await?;
        Ok(best)
    }

    /// Re-gate a submission this pass already reached, with no model call.
    ///
    /// The decisions are replayed but the **guardrail is not**: `gate` dry-runs against
    /// the live ABox, which may have moved while the pass was down, so an edit that was
    /// safe an hour ago can be refused now. Replaying the verdict instead of the decision
    /// is what that would get wrong.
    async fn apply_adjudication(
        &self,
        ontology_manager: &OntologyManager,
        candidates: &[Proposal],
        submission: AdjudicationResult,
        projected_abox: &[Triple],
    ) -> (AssemblePlan, Vec<String>) {
        match self
            .gate_submission(
                ontology_manager,
                candidates,
                &submission,
                projected_abox,
                &[],
            )
            .await
        {
            Ok((outcome, rejected)) => match self
                .validate(ontology_manager, outcome, projected_abox)
                .await
            {
                Ok(outcome) => (outcome, rejected),
                Err(error) => {
                    warn!(%error, "pkm assemble: re-gating the banked adjudication failed");
                    (AssemblePlan::default(), vec![error.to_string()])
                }
            },
            Err(e) => {
                warn!(error = %e, "pkm assemble: re-gating the banked adjudication failed");
                (AssemblePlan::default(), vec![e.to_string()])
            }
        }
    }

    /// Run one submission through the guardrail cumulatively: each term is
    /// dry-run against the real ABox *together with* what has already been accepted,
    /// because that is the edit set actually heading for the commit. Returns what cleared
    /// and the rejections to feed back.
    async fn gate_submission(
        &self,
        ontology_manager: &OntologyManager,
        candidates: &[Proposal],
        submission: &AdjudicationResult,
        projected_abox: &[Triple],
        accepted_edits: &[SchemaEdit],
    ) -> Result<(AssemblePlan, Vec<String>), AppError> {
        let user_id = &self.ctx.scope.user_id;
        let px = self.prefixes.clone();

        // Matched on the **term**, not on its spelling. This was an exact string compare,
        // and it is the reason a pass could commit nothing while reporting success: a
        // proposal listed as `forum_url` never matched the `frona:forum_url` the model had
        // just tested and submitted, every decision missed, and each miss was counted as a
        // deferral. Two spellings are the same term when they expand to the same IRI -
        // either as written, or after the repair that produced the proposal in the first
        // place, since the model may answer with the spelling it originally chose.
        let same_term = |a: &str, b: &str, kind: ProposalKind| {
            let (a, b) = (a.trim(), b.trim());
            if px.expand(a) == px.expand(b) {
                return true;
            }
            match (
                px.repair_term(a, kind.term_kind()),
                px.repair_term(b, kind.term_kind()),
            ) {
                (Ok(x), Ok(y)) => px.expand(&x) == px.expand(&y),
                _ => false,
            }
        };

        let mut outcome = AssemblePlan {
            edits: accepted_edits.to_vec(),
            ..Default::default()
        };
        let mut rejected: Vec<String> = Vec::new();
        let mut unanswered: Vec<&str> = Vec::new();
        for c in candidates {
            let matching: Vec<_> = submission
                .decisions
                .iter()
                .filter(|d| same_term(&d.term, &c.term, c.kind))
                .collect();
            let fallback = Decision::AcceptProposal;
            let decision = if matching.len() == 1 {
                &matching[0].decision
            } else {
                unanswered.push(&c.term);
                &fallback
            };
            // Missing, deferred, or invalid replacements fall back to the Classify's
            // globally validated baseline instead of stranding undeclared usage.
            let mut edits = if matches!(decision, Decision::AcceptProposal | Decision::Defer) {
                c.proposed_edits.clone()
            } else {
                decision.edits(&c.term, c.kind)
            };
            // A structural rewrite must not discard the Classify-authored intent.
            // Comments describe the proposed term itself and survive declare/align/merge.
            for annotation in c
                .proposed_edits
                .iter()
                .filter(|edit| matches!(edit, SchemaEdit::AnnotateComment { .. }))
            {
                if !edits.contains(annotation) {
                    edits.push(annotation.clone());
                }
            }
            // The stage holding the source-memory context owns semantic minting.
            // Adjudication may accept, align, merge, amend, or weaken that declaration,
            // but a `declare` cannot add an axiom that was absent from the validated
            // originating proposal merely because the current ABox happens to fit it.
            if let Err(reason) = validate_declaration_strengthening(
                decision,
                c,
                &outcome.edits,
                projected_abox,
                |term| px.expand(term),
            ) {
                rejected.push(format!(
                    "- `{}` (declare): {reason}; keep only its proposed axioms, add \
                     supporting assertions/types, or align/merge it",
                    c.term,
                ));
                continue;
            }
            if edits.is_empty() {
                outcome.note(&c.term, decision);
                continue;
            }
            let mut trial = outcome.edits.clone();
            if !matches!(decision, Decision::AcceptProposal | Decision::Defer) {
                trial.retain(|edit| !c.proposed_edits.contains(edit));
            }
            for e in edits {
                if !trial.contains(&e) {
                    trial.push(e);
                }
            }
            let impact = ontology_manager
                .test_edits_with_abox(
                    user_id,
                    &trial,
                    &validation_abox(projected_abox, &trial, &px),
                )
                .await?;
            match gate(&impact) {
                GateOutcome::Commit { .. } => {
                    outcome.edits = trial;
                    outcome.note(&c.term, decision);
                }
                GateOutcome::Incoherent => rejected.push(format!(
                    "- `{}` ({}): contradiction — the schema becomes unsatisfiable",
                    c.term,
                    decision.label()
                )),
                GateOutcome::DataViolations { affected } => rejected.push(format!(
                    "- `{}` ({}): would invalidate {affected} projected A-Box \
                     fact(s); every ontology edit must preserve the full graph. {}",
                    c.term,
                    decision.label(),
                    format_edit_impact(&impact),
                )),
            }
        }
        // Not folded into `rejected`: the model is not asked again for these, because a
        // proposal it never mentioned is deferred by design and re-surfaces next pass. But
        // it must not be *invisible* - an unanswered term and a term the model deliberately
        // deferred were indistinguishable, which is how a whole pass of empty commits read
        // as a pass of considered deferrals.
        if !unanswered.is_empty() {
            warn!(
                terms = %unanswered.join(", "),
                answered = submission.decisions.len(),
                proposed = candidates.len(),
                "adjudicate: proposals the submission did not decide"
            );
        }
        for nomination in &submission.amendment_nominations {
            let term = nomination.term.trim();
            if term.is_empty() || nomination.evidence.trim().is_empty() {
                continue;
            }
            let proposed_edits = vec![SchemaEdit::AmendOverride {
                target: nomination.target.clone(),
            }];
            if !outcome
                .nominations
                .iter()
                .any(|held| held.term == term && held.proposed_edits == proposed_edits)
            {
                outcome.nominations.push(Proposal {
                    term: term.into(),
                    kind: nomination.term_kind,
                    usage_entities: 0,
                    usage_links: 0,
                    description: "Existing axiom nominated for repair.".into(),
                    proposed_edits,
                });
            }
        }
        Ok((outcome, rejected))
    }

    /// Revalidate the complete batch after all of its individually gated edits have
    /// interacted. This is the batch commit boundary: neither a violation nor a
    /// validator failure may be converted into quarantine or ignored.
    async fn validate(
        &self,
        ontology_manager: &OntologyManager,
        outcome: AssemblePlan,
        projected_abox: &[Triple],
    ) -> Result<AssemblePlan, AppError> {
        if !outcome.edits.is_empty() {
            let impact = ontology_manager
                .test_edits_with_abox(
                    &self.ctx.scope.user_id,
                    &outcome.edits,
                    &validation_abox(projected_abox, &outcome.edits, &self.prefixes),
                )
                .await?;
            if !impact.incoherence.is_empty() || !impact.data_violations.is_empty() {
                return Err(AppError::Internal(format!(
                    "assemble: adjudication batch failed cumulative validation: {}",
                    format_edit_impact(&impact),
                )));
            }
        }
        Ok(outcome)
    }

    /// Plan the schema commit and every entity type it justifies, then write both in one
    /// transaction. Returns how many entities ended up typed.
    ///
    /// Three things go in together, because each is meaningless without the others: the
    /// delta, the `kinds` stamped against it, and the term renames an `align`/`merge`
    /// implies - including on entities from *earlier* passes still sitting on the superseded
    /// term. Splitting them is what let an entity carry `frona:Foo` while the TBox had never
    /// heard of it.
    ///
    /// A CAS miss means a concurrent writer moved the delta under us, so the whole plan
    /// is recomputed against the newer one rather than forced over it.
    async fn commit(
        &self,
        ontology_manager: &OntologyManager,
        entities: &[KnowledgeConsolidationEntity],
        proposals: &ProposalSet,
        outcome: &AssemblePlan,
        progress: &ConsolidationProgress<'_>,
    ) -> Result<usize, AppError> {
        let user_id = &self.ctx.scope.user_id;
        // `renames` carries CURIEs (what the model emits and what the delta records)
        // while `kinds` holds IRIs, so the two spellings have to be reconciled before
        // any lookup - comparing them raw silently matches nothing, which reads as
        // "no prior entities used it".
        let px = self.prefixes.clone();
        let pages_by_path: BTreeMap<&str, &KnowledgeConsolidationEntity> = entities
            .iter()
            .chain(proposals.staged_entities.values())
            .map(|entity| (entity.path.as_str(), entity))
            .collect();

        for attempt in 0..SCHEMA_COMMIT_ATTEMPTS {
            let planned = ontology_manager
                .plan_schema(user_id, &outcome.edits)
                .await?;
            // Entity path → the kinds it should end up with. A `BTreeMap` so an entity touched
            // by both a retype and a stamp accumulates rather than having one overwrite
            // the other, and so the write order is stable.
            let mut types: BTreeMap<String, Vec<String>> = BTreeMap::new();

            for (from, to) in &outcome.renames {
                let (from_iri, to_iri) = (px.expand(from), px.expand(to));
                for (path, kinds) in ontology_manager
                    .plan_retype(user_id, &from_iri, &to_iri, &planned.triples)
                    .await?
                {
                    types.insert(path, kinds);
                }
            }

            let mut rekeys: Vec<(String, String, String)> = Vec::new();
            let mut attributes: Vec<AttributeOps> = Vec::new();
            let mut stamped = 0;
            let mut paths: Vec<&String> = proposals.by_path.keys().collect();
            paths.sort();
            for path in paths {
                let Some(p) = proposals.by_path.get(path) else {
                    continue;
                };
                let Some(entity) = pages_by_path.get(path.as_str()).copied() else {
                    continue;
                };
                // Start from whatever the retype pass already decided for this entity, so a
                // entity that is both realigned and stamped sees both.
                let mut kinds = types
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| entity.kinds.clone());
                // Every proposed class through the gate on its own: one being refused
                // does not sink the others, and an entity that gained at least one counts.
                let mut gained = false;
                for class in &p.classes {
                    match ontology_manager.plan_entity_type(
                        &kinds,
                        outcome.final_term(class),
                        &planned.triples,
                    ) {
                        TypePlan::Write(next) => {
                            kinds = next;
                            gained = true;
                        }
                        TypePlan::AlreadyHeld => gained = true,
                        TypePlan::Refused => {}
                    }
                }
                if !gained {
                    continue;
                }
                types.insert(path.clone(), kinds);
                stamped += 1;
                for (from, to) in &p.rekeys {
                    rekeys.push((
                        path.clone(),
                        from.clone(),
                        outcome.final_term(to).to_string(),
                    ));
                }
                // `final_term` on both halves, so a term adjudicate aligned or merged is
                // what the attribute is re-keyed to and what the promoted edge is named -
                // otherwise the entity would carry a property the commit just renamed away.
                if !p.attr_rekeys.is_empty() || !p.promoted.is_empty() || !p.retracted.is_empty() {
                    attributes.push(AttributeOps {
                        path: path.clone(),
                        rekeys: p
                            .attr_rekeys
                            .iter()
                            .map(|(from, to)| (from.clone(), outcome.final_term(to).to_string()))
                            .collect(),
                        promoted: p
                            .promoted
                            .iter()
                            .map(|(key, to, target)| {
                                (
                                    key.clone(),
                                    outcome.final_term(to).to_string(),
                                    target.clone(),
                                )
                            })
                            .collect(),
                        retracted: p
                            .retracted
                            .iter()
                            .map(|(property, target)| {
                                (outcome.final_term(property).to_string(), target.clone())
                            })
                            .collect(),
                    });
                }
            }

            let types: Vec<(String, Vec<String>)> = types.into_iter().collect();
            let merge_targets: std::collections::HashSet<String> = progress
                .resolved_into()
                .values()
                .map(|path| progress.canonical_path(path))
                .collect();
            let materialize: Vec<KnowledgeEntity> = proposals
                .input_entities
                .values()
                .filter(|entity| {
                    proposals.by_path.contains_key(&entity.path)
                        || merge_targets.contains(&entity.path)
                })
                .cloned()
                .map(|entity| proposals.project_entity(entity))
                .map(|entity| entity.as_knowledge_entity())
                .collect();
            tracing::debug!(
                expected_version = planned.version,
                attribute_ops = ?attributes,
                relation_rekeys = ?rekeys,
                "pkm assemble: committing mapped properties"
            );
            let mut completed_checkpoint = self.ctx.record().await;
            completed_checkpoint.stats.entities_created += proposals
                .staged_entities
                .values()
                .filter(|entity| proposals.by_path.contains_key(&entity.path))
                .filter(|entity| !progress.resolved_into().contains_key(&entity.path))
                .count();
            completed_checkpoint.state = completed_checkpoint.state.next();
            completed_checkpoint.attempts = 0;
            completed_checkpoint.updated_at = chrono::Utc::now();
            let coalesced_sources: Vec<(String, String, String)> = progress
                .resolved_into()
                .iter()
                .flat_map(|(from, into)| {
                    progress.entity_row(from).into_iter().flat_map(move |row| {
                        row.source_memory_ids.iter().map(move |memory_id| {
                            (
                                from.clone(),
                                progress.canonical_path(into),
                                memory_id.clone(),
                            )
                        })
                    })
                })
                .collect();
            let coalesced_aliases: Vec<(String, Vec<String>)> = progress
                .resolved_into()
                .iter()
                .filter_map(|(from, into)| {
                    progress.entity_row(from).map(|row| {
                        let mut aliases = row.aliases.clone();
                        if !row.name.trim().is_empty() {
                            aliases.insert(row.name.clone());
                        }
                        (progress.canonical_path(into), aliases.into_iter().collect())
                    })
                })
                .collect();
            let relation_type_renames: Vec<(String, String)> = outcome
                .renames
                .iter()
                .map(|(from, to)| (from.clone(), to.clone()))
                .collect();
            let mut working_outcomes: Vec<(String, Option<String>)> = progress
                .resolved_into()
                .iter()
                .map(|(from, into)| (from.clone(), Some(into.clone())))
                .collect();
            working_outcomes.extend(progress.discarded_paths().map(|path| (path.clone(), None)));
            if self
                .ctx
                .repo
                .commit_schema_and_types(
                    user_id,
                    &planned.owl,
                    crate::memory::pkm::ontology::DELTA_FORMAT,
                    planned.version,
                    &types,
                    &rekeys,
                    &relation_type_renames,
                    &attributes,
                    &materialize,
                    &coalesced_sources,
                    &coalesced_aliases,
                    &working_outcomes,
                    Some(&completed_checkpoint),
                )
                .await?
            {
                self.ctx.adopt_committed_record(completed_checkpoint).await;
                return Ok(stamped);
            }
            warn!(
                attempt = attempt + 1,
                "pkm assemble: schema CAS miss, re-planning"
            );
        }
        Err(AppError::Conflict(
            "assemble: schema commit exceeded its CAS retry budget".into(),
        ))
    }
}

fn format_edit_impact(impact: &crate::memory::pkm::ontology::EditImpact) -> String {
    let mut diagnostics = impact
        .incoherence
        .iter()
        .map(|detail| format!("T-Box: {detail}"))
        .collect::<Vec<_>>();
    diagnostics.extend(impact.data_violations.iter().map(|violation| {
        format!(
            "A-Box: rule={} subject={} detail={}",
            violation.rule,
            violation.subject.as_deref().unwrap_or("unknown"),
            violation.detail,
        )
    }));
    diagnostics.join("; ")
}

/// Materialize the assertion aliases that the accepted equivalence edits imply before
/// asking the validator to judge the batch. The reasoner handles OWL equivalence, but
/// datatype facets are checked directly over predicates; without these aliases an Align
/// can look clean here and fail only after commit re-keys the entity to its final property.
fn validation_abox(
    projected: &[Triple],
    edits: &[SchemaEdit],
    prefixes: &crate::memory::pkm::ontology::PrefixMap,
) -> Vec<Triple> {
    let mut out = projected.to_vec();
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let mut property_pairs = Vec::new();
    let mut class_pairs = Vec::new();
    for edit in edits {
        match edit {
            SchemaEdit::Align {
                frona,
                standard,
                kind: AlignKind::Class,
            }
            | SchemaEdit::EquivalentClasses {
                a: frona,
                b: standard,
            } => {
                class_pairs.push((prefixes.expand(frona), prefixes.expand(standard)));
            }
            SchemaEdit::Align {
                frona,
                standard,
                kind: AlignKind::DataProperty | AlignKind::ObjectProperty,
            }
            | SchemaEdit::EquivalentProperties {
                a: frona,
                b: standard,
            } => {
                property_pairs.push((prefixes.expand(frona), prefixes.expand(standard)));
            }
            _ => {}
        }
    }
    loop {
        let before = out.len();
        let snapshot = out.clone();
        for triple in snapshot {
            for (a, b) in &property_pairs {
                let replacement = if triple.predicate.as_str() == a {
                    Some(b)
                } else if triple.predicate.as_str() == b {
                    Some(a)
                } else {
                    None
                };
                if let Some(replacement) = replacement {
                    let alias = Triple::new(
                        triple.subject.clone(),
                        oxrdf::NamedNode::new_unchecked(replacement),
                        triple.object.clone(),
                    );
                    if !out.contains(&alias) {
                        out.push(alias);
                    }
                }
            }
            if triple.predicate.as_str() == rdf_type
                && let oxrdf::Term::NamedNode(class) = &triple.object
            {
                for (a, b) in &class_pairs {
                    let replacement = if class.as_str() == a {
                        Some(b)
                    } else if class.as_str() == b {
                        Some(a)
                    } else {
                        None
                    };
                    if let Some(replacement) = replacement {
                        let alias = Triple::new(
                            triple.subject.clone(),
                            triple.predicate.clone(),
                            oxrdf::Term::NamedNode(oxrdf::NamedNode::new_unchecked(replacement)),
                        );
                        if !out.contains(&alias) {
                            out.push(alias);
                        }
                    }
                }
            }
        }
        if out.len() == before {
            break;
        }
    }
    out
}

fn validate_declaration_strengthening(
    decision: &Decision,
    proposal: &Proposal,
    accepted_edits: &[SchemaEdit],
    projected_abox: &[Triple],
    expand: impl Fn(&str) -> String,
) -> Result<(), String> {
    if !matches!(decision, Decision::Declare { .. }) {
        return Ok(());
    }
    let added: Vec<_> = decision
        .edits(&proposal.term, proposal.kind)
        .into_iter()
        .filter(|edit| !proposal.proposed_edits.contains(edit))
        .collect();
    if added.is_empty() {
        return Ok(());
    }

    let equivalent_properties = |term: &str| {
        let mut terms = HashSet::from([expand(term)]);
        loop {
            let before = terms.len();
            for edit in accepted_edits.iter().chain(proposal.proposed_edits.iter()) {
                let pair = match edit {
                    SchemaEdit::EquivalentProperties { a, b } => Some((a, b)),
                    SchemaEdit::Align {
                        frona,
                        standard,
                        kind: AlignKind::ObjectProperty,
                    } => Some((frona, standard)),
                    _ => None,
                };
                if let Some((a, b)) = pair {
                    let (a, b) = (expand(a), expand(b));
                    if terms.contains(&a) {
                        terms.insert(b.clone());
                    }
                    if terms.contains(&b) {
                        terms.insert(a);
                    }
                }
            }
            if terms.len() == before {
                break;
            }
        }
        terms
    };
    let assertions_for = |property: &str| {
        let aliases = equivalent_properties(property);
        projected_abox
            .iter()
            .filter(|triple| aliases.contains(triple.predicate.as_str()))
            .collect::<Vec<_>>()
    };
    let has_type = |term: &oxrdf::NamedOrBlankNode, class: &str| {
        let class = expand(class);
        projected_abox.iter().any(|triple| {
            triple.subject == *term
                && triple.predicate.as_str()
                    == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
                && matches!(&triple.object, oxrdf::Term::NamedNode(object) if object.as_str() == class)
        })
    };

    for edit in added {
        let supported = match &edit {
            SchemaEdit::ObjectPropertyDomain { property, class } => {
                let assertions = assertions_for(property);
                !assertions.is_empty()
                    && assertions
                        .iter()
                        .all(|triple| has_type(&triple.subject, class))
            }
            SchemaEdit::ObjectPropertyRange { property, class } => {
                let assertions = assertions_for(property);
                !assertions.is_empty()
                    && assertions.iter().all(|triple| match &triple.object {
                        oxrdf::Term::NamedNode(object) => {
                            has_type(&oxrdf::NamedOrBlankNode::NamedNode(object.clone()), class)
                        }
                        _ => false,
                    })
            }
            SchemaEdit::InverseProperties { a, b } => {
                let a = assertions_for(a);
                let b_aliases = equivalent_properties(b);
                !a.is_empty()
                    && a.iter().all(|forward| {
                        let oxrdf::Term::NamedNode(object) = &forward.object else {
                            return false;
                        };
                        projected_abox.iter().any(|reverse| {
                            reverse.subject == oxrdf::NamedOrBlankNode::NamedNode(object.clone())
                                && reverse.object == oxrdf::Term::from(forward.subject.clone())
                                && b_aliases.contains(reverse.predicate.as_str())
                        })
                    })
            }
            _ => false,
        };
        if !supported {
            return Err(format!(
                "added axiom {edit:?} is not supported by the projected ABox"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
