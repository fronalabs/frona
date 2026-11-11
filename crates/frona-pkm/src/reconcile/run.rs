use super::{
    inspection::ReconciliationInspection,
    projection::{
        curie_key, curie_key_attributes, drop_keys_held_as_relations, render_attribute_lines,
        render_memory_lines,
    },
    validation::{
        accepted_promotions, merge_explicit_retractions, replace_is_supported,
        replacement_retractions,
    },
    *,
};
use crate::prompt::PromptIds;
use crate::{
    Error,
    classify::ProposalSet,
    model::{
        AttributeSource, EntityCategory, KnowledgeEntityLink, KnowledgeMemory, RelationType,
        SELF_ENTITY_PATH, terminal_task_plan_ids,
    },
    ontology::{PrefixMap, TermKind},
    path::normalize_path,
};
use frona_harness::{FailureKind, Harness, Run, RunOutcome, Text};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::AtomicBool},
};
use tracing::warn;

pub(super) struct ReconcileRun<'a> {
    pub input: Arc<ReconcileInput>,
    pub prefixes: PrefixMap,
    pub prompts: ReconcilePrompts,
    pub harness: &'a Harness<'a>,
    pub max_submissions: usize,
}
#[derive(Clone)]
pub(super) struct ReconcileLogic {
    pub input: Arc<ReconcileInput>,
    pub prefixes: PrefixMap,
    pub prompts: ReconcilePrompts,
}
pub(super) fn partition_reconcile_memories(
    all_memories: &[KnowledgeMemory],
) -> (Vec<KnowledgeMemory>, Vec<KnowledgeMemory>) {
    let terminal_task_plans = terminal_task_plan_ids(all_memories);
    let current = all_memories
        .iter()
        .filter(|memory| {
            memory.relations.is_empty()
                && memory.disposition == crate::model::Disposition::None
                && !terminal_task_plans.contains(&memory.id)
        })
        .cloned()
        .collect();
    let historical = all_memories
        .iter()
        .filter(|memory| {
            !memory.relations.is_empty()
                || memory.disposition != crate::model::Disposition::None
                || terminal_task_plans.contains(&memory.id)
        })
        .cloned()
        .collect();
    (current, historical)
}

impl ReconcileLogic {
    pub(super) async fn reconcile_trial(
        &self,
        path: &str,
        entity: &crate::model::KnowledgeConsolidationEntity,
        links: &[KnowledgeEntityLink],
        memories: &[KnowledgeMemory],
        verdict: &EntityVerdict,
        proposals: &ProposalSet,
    ) -> Result<ProposalSet, Error> {
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
                    .input
                    .graph
                    .entity_by_path_with(&draft, &link.to_entity_path)?;
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
            &self.input,
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
            &self.input,
            proposals,
            path,
        )
        .await?;
        let retractions = merge_explicit_retractions(retractions, verdict);
        trial.add_reconcile_promotions(path, &promotions, &retractions, &self.prefixes);
        Ok(trial)
    }
}
impl<'a> ReconcileRun<'a> {
    pub async fn run<F>(&self, configure: F) -> Result<ReconcileOutput, Error>
    where
        F: FnOnce(Run<'a, Text>) -> Run<'a, Text> + Send,
    {
        let path = self.input.path.as_str();
        let mut proposals = self.input.proposals.clone();
        let Some(entity) = self
            .input
            .graph
            .entity_by_path_with(&proposals.entity_draft(), path)?
        else {
            return Ok(ReconcileOutput {
                proposals,
                ..Default::default()
            });
        };
        if entity.category != EntityCategory::Concept || entity.source_memory_ids.is_empty() {
            return Ok(ReconcileOutput {
                proposals,
                ..Default::default()
            });
        }
        proposals.stage_input_entity(entity.clone());
        let proposals = &proposals;
        let initial_rejection = self.input.initial_rejection.as_deref();
        let all_memories: Vec<_> = self
            .input
            .graph
            .memories
            .iter()
            .filter(|memory| {
                entity.source_memory_ids.contains(&memory.id)
                    || self
                        .input
                        .graph
                        .memory_entities
                        .get(&memory.id)
                        .is_some_and(|paths| paths.iter().any(|held| held == path))
            })
            .cloned()
            .collect();
        // Current memories are mutable verdict subjects. Historical memories are rendered
        // only so an Agent retrieval echo can be recognized without resurrecting them.
        // A terminal task event also makes the same task's planned episode historical;
        // the stable task identity comes from structured lifecycle evidence.
        let (memories, historical) = partition_reconcile_memories(&all_memories);
        if memories.is_empty() {
            let mut accepted = proposals.clone();
            accepted.apply_reconciled_entity(
                path,
                String::new(),
                "(no memories yet)".into(),
                serde_json::json!({}),
                Vec::new(),
            );
            let validation = crate::projection::validate_proposal_projection(
                &self.input.graph,
                &self.input.ontology,
                &accepted,
            )
            .await?;
            if !validation.is_valid() {
                return Err(Error::Internal(
                    crate::projection::projection_rejection_details(&validation),
                ));
            }
            return Ok(ReconcileOutput {
                proposals: accepted,
                entity_update: Some(ReconciledEntityUpdate {
                    name: String::new(),
                    description: "(no memories yet)".into(),
                    attributes: serde_json::json!({}),
                    attribute_sources: Vec::new(),
                    declarations: Vec::new(),
                }),
                ..ReconcileOutput::default()
            });
        }

        let prompt_ids = PromptIds::new("m", all_memories.iter().map(|memory| memory.id.clone()));
        let mut memory_entities = HashMap::<String, Vec<String>>::new();
        for memory in &all_memories {
            let mut entities = self
                .input
                .graph
                .memory_entities
                .get(&memory.id)
                .cloned()
                .ok_or_else(|| {
                    Error::Internal(format!(
                        "reconcile snapshot is missing the scope of memory `{}`",
                        memory.id
                    ))
                })?;
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
            &self.input.graph.scope_id,
            path,
            self.input
                .graph
                .links
                .iter()
                .filter(|link| link.from_entity_path == path)
                .cloned()
                .collect(),
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
        let (system, mut input) = self.prompts.render(vars)?;
        input.push_str(&suffix);
        let run = configure(self.harness.run(input).instructions(system))
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
        let catalogue = self
            .input
            .ontology
            .catalog(&self.input.graph.scope_id)
            .await
            .ok();
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
                crate::ontology::SchemaEdit::DeclareObjectProperty { property } => {
                    known_object_properties.insert(property.clone());
                }
                crate::ontology::SchemaEdit::DeclareDataProperty { property } => {
                    known_data_properties.insert(property.clone());
                }
                _ => {}
            }
        }
        let draft = proposals.entity_draft();
        let suggestion_entities = self.input.graph.snapshot_with(&draft)?.into_entities();
        let inspection = Arc::new(ReconciliationInspection {
            reconciler: ReconcileLogic {
                input: self.input.clone(),
                prefixes: self.prefixes.clone(),
                prompts: self.prompts.clone(),
            },
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
                    return Err(Error::Cancelled(incomplete.reason.to_string()));
                }
                return Err(Error::Internal(format!(
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
        let mut commit = ReconcileChanges::default();

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
                commit.memory_relations.push(MemoryRelationChange {
                    subordinate_id: sub.to_string(),
                    relation: link.relation,
                    to_id: to.to_string(),
                    note: link.note.trim().to_string(),
                });
                retired_any = true;
            }
        }

        for o in &parsed.outdated {
            let m = o.memory.trim();
            if !valid.contains(m) {
                continue;
            }
            commit.outdated_memories.push(OutdatedMemory {
                memory_id: m.to_string(),
                reason: o.note.trim().to_string(),
            });
            retired_any = true;
        }

        if retired_any {
            // The description and attributes were derived from a memory set we just
            // changed, so defer those until the clean rerun. Relation transitions are
            // different: they describe the same accepted state change and must not be
            // discarded, because the old memory will be historical on that rerun.
            let promotions = accepted_promotions(
                &parsed,
                &self.input,
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
                &self.input,
                proposals,
                path,
            )
            .await?;
            let retractions = merge_explicit_retractions(retractions, &parsed);
            reconcile_dirty.insert(path.to_string());
            return Ok(ReconcileOutput {
                reconcile_dirty,
                promotions,
                retractions,
                entity_update: None,
                changes: commit,
                profile_attributes: None,
                move_entity: None,
                proposals: ReconcileLogic {
                    input: self.input.clone(),
                    prefixes: self.prefixes.clone(),
                    prompts: self.prompts.clone(),
                }
                .reconcile_trial(path, &entity, &links, &memories, &parsed, proposals)
                .await?,
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
                .input
                .graph
                .entity_by_path_with(&draft, &l.to_entity_path)?;
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
            &self.input,
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
            &self.input,
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
            commit.entity_link_sources.push(EntityLinkSources {
                from_entity_path: path.to_string(),
                to_entity_path: relation.target.trim().to_string(),
                relation: property,
                source_memory_ids: relation.source_memory_ids.clone(),
            });
        }

        let move_entity = parsed.moves.first().and_then(|mv| {
            let (from, to) = (normalize_path(&mv.from)?, normalize_path(&mv.to)?);
            (from == path && from != to).then_some(EntityMove { from, to })
        });
        let move_entity = match move_entity {
            Some(mv)
                if self
                    .input
                    .graph
                    .entity_by_path_with(&proposals.entity_draft(), &mv.to)?
                    .is_none() =>
            {
                Some(mv)
            }
            _ => None,
        };
        Ok(ReconcileOutput {
            reconcile_dirty,
            promotions,
            retractions,
            entity_update: Some(ReconciledEntityUpdate {
                name: parsed.name.trim().to_string(),
                description,
                attributes,
                attribute_sources,
                declarations: parsed.declarations.clone(),
            }),
            changes: commit,
            profile_attributes: (path == SELF_ENTITY_PATH).then(|| parsed.attributes.clone()),
            move_entity,
            proposals: ReconcileLogic {
                input: self.input.clone(),
                prefixes: self.prefixes.clone(),
                prompts: self.prompts.clone(),
            }
            .reconcile_trial(path, &entity, &links, &memories, &parsed, proposals)
            .await?,
        })
    }
}
