use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::Utc;

use crate::classify::proposal::schema::{EntityShape, HasKeyMarker, OntologyDeclaration};
use crate::model::{
    KnowledgeConsolidationEntity, KnowledgeEntity, KnowledgeEntityLink, LinkOrigin,
};
use crate::ontology::{SchemaEdit, TermKind};
use crate::reconcile::ReconcilePromotion;

#[derive(Debug, Clone, Default)]
pub struct EntityProposal {
    /// The class CURIEs proposed for this entity.
    pub classes: Vec<String>,
    /// The schema edits the classification implies (a `frona:` class mint under a
    /// standard parent, object-property declarations, inverses).
    pub edits: Vec<SchemaEdit>,
    /// `(free_text_relation, curie)` re-keys to apply when the entity is stamped.
    pub rekeys: Vec<(String, String)>,
    /// `(free_text_key, curie)` - attributes that stay attributes, re-keyed to a CURIE.
    pub attr_rekeys: Vec<(String, String)>,
    /// `(free_text_key, curie, target_path)` - attributes whose value names another entity,
    /// so they become an edge and stop being a literal.
    pub promoted: Vec<(String, String, String)>,
    pub promoted_sources: HashMap<(String, String), Vec<String>>,
    /// `(relation_curie, target_path)` asserted edges removed at final commit.
    pub retracted: Vec<(String, String)>,
    pub has_keys: Vec<HasKeyMarker>,
    pub inverse_functional_properties: Vec<String>,
}

/// Every proposal this pass has made, plus the schema layer they imply.
#[derive(Debug, Clone, Default)]
pub struct ProposalSet {
    pub by_path: HashMap<String, EntityProposal>,
    /// The entities in the current in-memory semantic proposal. Durable accepted state
    /// remains on consolidation entity rows.
    pub input_entities: HashMap<String, KnowledgeConsolidationEntity>,
    /// Entities minted by classification itself. Like extracted candidates, these are
    /// virtual until the schema/entity commit.
    pub staged_entities: HashMap<String, KnowledgeConsolidationEntity>,
    /// Every edit the pass has proposed, deduped, in proposal order - the schema layer
    /// composed over the committed delta.
    /// The classify satisfaction loop and resolve's reasoning both compose this over the
    /// committed delta, so each entity is judged against what the pass has already
    /// proposed rather than against the stored schema alone.
    pub proposed_edits: Vec<SchemaEdit>,
    /// Semantic intent authored where each term was minted.
    pub declaration_descriptions: HashMap<String, String>,
    pub entity_shapes: HashMap<String, EntityShape>,
    /// Entities whose attributes have passed Reconcile. Raw extractor attributes are not
    /// A-box assertions; these are, so `reasoning_entities` must not restore their baseline.
    pub reconciled_paths: HashSet<String>,
}

impl ProposalSet {
    pub fn provisional_has_keys(&self) -> Vec<HasKeyMarker> {
        let mut out = Vec::new();
        for marker in self
            .by_path
            .values()
            .flat_map(|proposal| &proposal.has_keys)
        {
            if !out.contains(marker) {
                out.push(marker.clone());
            }
        }
        out
    }

    pub fn provisional_inverse_functional_properties(&self) -> BTreeSet<String> {
        self.by_path
            .values()
            .flat_map(|proposal| proposal.inverse_functional_properties.iter().cloned())
            .collect()
    }

    pub fn referenced_targets(&self) -> HashSet<String> {
        self.by_path
            .values()
            .flat_map(|proposal| {
                proposal
                    .promoted
                    .iter()
                    .map(|(_, _, target)| target.clone())
            })
            .collect()
    }

    pub fn trace_value(&self) -> serde_json::Value {
        let entities: serde_json::Map<String, serde_json::Value> = self
            .by_path
            .iter()
            .map(|(path, proposal)| {
                (
                    path.clone(),
                    serde_json::json!({
                        "classes": proposal.classes,
                        "schema_edits": proposal.edits,
                        "relation_rekeys": proposal.rekeys,
                        "attribute_rekeys": proposal.attr_rekeys,
                        "promoted_relations": proposal.promoted,
                        "retracted_relations": proposal.retracted,
                        "has_keys": proposal.has_keys,
                        "inverse_functional_properties": proposal.inverse_functional_properties,
                    }),
                )
            })
            .collect();
        serde_json::json!({
            "entities": entities,
            "entity_shapes": self.entity_shapes,
            "input_entities": self.input_entities,
            "staged_entities": self.staged_entities,
            "proposed_schema_edits": self.proposed_edits,
            "ontology_declaration_intent": self.declaration_descriptions,
            "reconciled_paths": self.reconciled_paths,
        })
    }

    pub fn stage_entity(&mut self, entity: KnowledgeConsolidationEntity) {
        self.input_entities
            .insert(entity.path.clone(), entity.clone());
        self.staged_entities.insert(entity.path.clone(), entity);
    }

    pub fn stage_input_entity(&mut self, entity: KnowledgeConsolidationEntity) {
        self.input_entities.insert(entity.path.clone(), entity);
    }

    pub fn input_entity(&self, path: &str) -> Option<KnowledgeConsolidationEntity> {
        self.input_entities
            .get(path)
            .cloned()
            .map(|entity| self.project_entity(entity))
    }

    pub fn entity_draft(&self) -> crate::view::EntityDraft {
        crate::view::EntityDraft::from_rows(
            self.input_entities
                .values()
                .cloned()
                .map(|entity| self.project_entity(entity)),
        )
    }

    /// Build the A-box-visible entity set. Extractor attributes remain evidence until a
    /// Classify proposal maps them to a validated data property; only committed
    /// baseline attributes and accepted mappings may participate in reasoning.
    pub fn reasoning_entities(&self, live_entities: Vec<KnowledgeEntity>) -> Vec<KnowledgeEntity> {
        let live_by_path: HashMap<_, _> = live_entities
            .iter()
            .cloned()
            .map(|entity| (entity.path.clone(), entity))
            .collect();
        let staged_paths: HashSet<_> = self.input_entities.keys().cloned().collect();
        let mut entities: Vec<_> = self
            .input_entities
            .values()
            // Extraction-only identity shells are Resolve evidence, not entity assertions.
            // If a classified relation points at one, the asserted edge itself introduces
            // that IRI into the A-box; merely naming a candidate must not do so.
            .filter(|entity| !(entity.entity_id.is_none() && entity.source_memory_ids.is_empty()))
            .cloned()
            .map(|mut entity| {
                if !self.reconciled_paths.contains(&entity.path) {
                    entity.attributes = live_by_path
                        .get(&entity.path)
                        .map(|live| live.attributes.clone())
                        .unwrap_or_else(|| serde_json::json!({}));
                }
                self.project_reasoning_entity(entity).as_knowledge_entity()
            })
            .collect();
        entities.extend(
            live_entities
                .into_iter()
                .filter(|entity| !staged_paths.contains(&entity.path))
                .map(|entity| KnowledgeConsolidationEntity::from_committed("baseline", entity))
                .map(|entity| self.project_reasoning_entity(entity).as_knowledge_entity()),
        );
        entities
    }

    /// Project the complete candidate entity/link graph represented by this snapshot.
    ///
    /// Resolve has already folded identity merges and target moves into `self`. Entity
    /// projection then applies accepted shapes, classes, attribute rekeys, and promoted
    /// attribute removal. Link projection applies relation rekeys, adds promoted edges,
    /// and finally removes explicit retractions. Validation and publication must use this
    /// operation so they reason over the same graph that the final commit will produce.
    pub fn project_graph(
        &self,
        user_id: &str,
        live_entities: Vec<KnowledgeEntity>,
        mut live_links: Vec<KnowledgeEntityLink>,
    ) -> (Vec<KnowledgeEntity>, Vec<KnowledgeEntityLink>) {
        let entities = self.reasoning_entities(live_entities);
        for entity in &entities {
            let from = live_links
                .iter()
                .filter(|link| link.from_entity_path == entity.path)
                .cloned()
                .collect();
            live_links.retain(|link| link.from_entity_path != entity.path);
            live_links.extend(self.project_links(user_id, &entity.path, from));
        }
        (entities, live_links)
    }

    fn project_reasoning_entity(
        &self,
        entity: KnowledgeConsolidationEntity,
    ) -> KnowledgeConsolidationEntity {
        let path = entity.path.clone();
        let mut entity = self.project_entity(entity);
        let (Some(raw), Some(proposal)) = (self.input_entities.get(&path), self.by_path.get(&path))
        else {
            return entity;
        };
        let (Some(raw_attributes), Some(projected_attributes)) = (
            raw.attributes.as_object(),
            entity.attributes.as_object_mut(),
        ) else {
            return entity;
        };
        for (from, to) in &proposal.attr_rekeys {
            let Some(value) = raw_attributes.get(from) else {
                continue;
            };
            match projected_attributes.get_mut(to) {
                Some(existing) => {
                    crate::model::merge_consolidation_attribute_values(existing, value)
                }
                None => {
                    projected_attributes.insert(to.clone(), value.clone());
                }
            }
        }
        entity
    }

    pub fn input_paths(&self) -> impl Iterator<Item = &String> {
        self.input_entities.keys()
    }

    pub fn memory_paths(&self, memory_id: &str) -> Vec<String> {
        self.input_entities
            .values()
            .filter(|entity| entity.source_memory_ids.iter().any(|id| id == memory_id))
            .map(|entity| entity.path.clone())
            .collect()
    }

    pub fn apply_reconciled_entity(
        &mut self,
        path: &str,
        name: String,
        description: String,
        attributes: serde_json::Value,
        attribute_sources: Vec<crate::model::AttributeSource>,
    ) {
        self.reconciled_paths.insert(path.to_string());
        if let Some(shape) = self.entity_shapes.get_mut(path) {
            if !name.trim().is_empty() {
                shape.name = name.clone();
            }
            shape.description = description.clone();
        }
        for entities in [&mut self.input_entities, &mut self.staged_entities] {
            if let Some(entity) = entities.get_mut(path) {
                if !name.trim().is_empty() {
                    entity.name = name.clone();
                }
                entity.description = description.clone();
                entity.attributes = attributes.clone();
                entity.attribute_sources = attribute_sources.clone();
                entity.rederive_search();
            }
        }
    }

    /// Materialize the entity fields already accepted by earlier Consolidator iterations.
    /// Reconcile must consume this view rather than the persisted pre-commit entity.
    pub fn project_entity(
        &self,
        mut entity: KnowledgeConsolidationEntity,
    ) -> KnowledgeConsolidationEntity {
        if let Some(shape) = self.entity_shapes.get(&entity.path) {
            entity.name = shape.name.clone();
            entity.description = shape.description.clone();
            entity.aliases = shape.aliases.iter().cloned().collect();
            entity.search_text = crate::model::derive_search_text(
                &entity.name,
                &entity.description,
                &entity.aliases,
            );
        }
        let Some(proposal) = self.by_path.get(&entity.path) else {
            return entity;
        };
        for class in &proposal.classes {
            if !entity.kinds.contains(class) {
                entity.kinds.push(class.clone());
            }
        }
        if let Some(attributes) = entity.attributes.as_object_mut() {
            for (old, new) in &proposal.attr_rekeys {
                if old != new
                    && let Some(value) = attributes.remove(old)
                {
                    attributes.insert(new.clone(), value);
                }
            }
            for (key, _, _) in &proposal.promoted {
                attributes.remove(key);
            }
        }
        entity
    }

    /// Materialize accepted relation rekeys and promotions for one entity.
    pub fn project_links(
        &self,
        user_id: &str,
        path: &str,
        mut links: Vec<KnowledgeEntityLink>,
    ) -> Vec<KnowledgeEntityLink> {
        let Some(proposal) = self.by_path.get(path) else {
            return links;
        };
        for (old, new) in &proposal.rekeys {
            for link in links
                .iter_mut()
                .filter(|link| link.origin == LinkOrigin::Asserted && link.relation == *old)
            {
                link.relation = new.clone();
            }
        }
        for (_, relation, target) in &proposal.promoted {
            if links.iter().any(|link| {
                link.origin == LinkOrigin::Asserted
                    && link.relation == *relation
                    && link.to_entity_path == *target
            }) {
                continue;
            }
            links.push(KnowledgeEntityLink {
                id: String::new(),
                user_id: user_id.to_string(),
                from_entity_path: path.to_string(),
                to_entity_path: target.clone(),
                relation: relation.clone(),
                source_memory_ids: proposal
                    .promoted_sources
                    .get(&(relation.clone(), target.clone()))
                    .cloned()
                    .unwrap_or_default(),
                origin: LinkOrigin::Asserted,
                created_at: Utc::now(),
            });
        }
        links.retain(|link| {
            !proposal.retracted.iter().any(|(relation, target)| {
                link.origin == LinkOrigin::Asserted
                    && link.relation == *relation
                    && link.to_entity_path == *target
            })
        });
        links
    }

    pub fn promotions_for(&self, path: &str) -> &[(String, String, String)] {
        self.by_path
            .get(path)
            .map(|proposal| proposal.promoted.as_slice())
            .unwrap_or(&[])
    }

    pub fn record(&mut self, path: &str, proposal: EntityProposal) {
        for e in &proposal.edits {
            if !self.proposed_edits.contains(e) {
                self.proposed_edits.push(e.clone());
            }
        }
        self.by_path.insert(path.to_string(), proposal);
    }

    pub fn record_declarations(&mut self, declarations: &[OntologyDeclaration]) {
        for declaration in declarations {
            self.declaration_descriptions.insert(
                declaration.term().to_string(),
                declaration.description().to_string(),
            );
        }
    }

    /// Fold properties minted by Reconcile into the staged T-box before checkpointing
    /// the corresponding entity update.
    pub fn add_reconcile_attributes(
        &mut self,
        path: &str,
        attributes: &serde_json::Value,
        declarations: &[OntologyDeclaration],
        px: &crate::ontology::PrefixMap,
    ) {
        let mut edits = Vec::new();
        for declaration in declarations {
            self.declaration_descriptions.insert(
                declaration.term().to_string(),
                declaration.description().to_string(),
            );
            edits.extend(declaration.edits());
        }
        if let Some(attributes) = attributes.as_object() {
            for key in attributes.keys() {
                let Ok(property) = px.repair_term(key, TermKind::Property) else {
                    continue;
                };
                if px.expand(&property).starts_with("urn:frona:")
                    && !edits.iter().any(|edit| {
                        matches!(
                            edit, SchemaEdit::DeclareDataProperty { property: held }
                                if px.expand(held) == px.expand(&property)
                        )
                    })
                {
                    edits.push(SchemaEdit::DeclareDataProperty { property });
                }
            }
        }
        let proposal = self.by_path.entry(path.to_string()).or_default();
        for edit in edits {
            if !proposal.edits.contains(&edit) {
                proposal.edits.push(edit);
            }
        }
        self.rebuild_edits();
    }

    /// Fold reconcile's fail-safe promotions into the same proposal set classify uses.
    /// A promoted key is no longer a literal, so retire classify's data-property decision
    /// for that key before adding the object-property operation.
    pub fn add_reconcile_promotions(
        &mut self,
        path: &str,
        promotions: &[ReconcilePromotion],
        retractions: &[(String, String)],
        px: &crate::ontology::PrefixMap,
    ) {
        if promotions.is_empty() && retractions.is_empty() {
            return;
        }
        let proposal = self.by_path.entry(path.to_string()).or_default();
        for retraction in retractions {
            if !proposal.retracted.contains(retraction) {
                proposal.retracted.push(retraction.clone());
            }
        }
        let mut retired_data_terms = Vec::new();
        for promotion in promotions {
            let (key, property, target) = (&promotion.key, &promotion.property, &promotion.target);
            proposal.attr_rekeys.retain(|(from, to)| {
                if from == key {
                    retired_data_terms.push(to.clone());
                    false
                } else {
                    true
                }
            });
            let promoted = (key.clone(), property.clone(), target.clone());
            if !proposal
                .promoted
                .iter()
                .any(|(_, held_property, held_target)| {
                    held_property == property && held_target == target
                })
            {
                proposal.promoted.push(promoted);
            }
            proposal.promoted_sources.insert(
                (property.clone(), target.clone()),
                promotion.source_memory_ids.clone(),
            );
            if px.expand(property).starts_with("urn:frona:") {
                let declaration = promotion.declaration.as_ref().and_then(|value| {
                    serde_json::from_value::<OntologyDeclaration>(value.clone()).ok()
                });
                if let Some(declaration) = declaration {
                    self.declaration_descriptions.insert(
                        declaration.term().to_string(),
                        declaration.description().to_string(),
                    );
                    for edit in declaration.edits() {
                        if !proposal.edits.contains(&edit) {
                            proposal.edits.push(edit);
                        }
                    }
                } else {
                    let edit = SchemaEdit::DeclareObjectProperty {
                        property: property.clone(),
                    };
                    if !proposal.edits.contains(&edit) {
                        proposal.edits.push(edit);
                    }
                }
            }
        }
        proposal.edits.retain(|edit| {
            !matches!(
                edit,
                SchemaEdit::DeclareDataProperty { property }
                    if retired_data_terms.contains(property)
            )
        });
        self.rebuild_edits();
    }

    fn rebuild_edits(&mut self) {
        self.proposed_edits.clear();
        for proposal in self.by_path.values() {
            for edit in &proposal.edits {
                if !self.proposed_edits.contains(edit) {
                    self.proposed_edits.push(edit.clone());
                }
            }
        }
    }

    /// Drop an entity's proposal - it was merged away and no longer exists to stamp. Its
    /// edits stay in the proposed layer: the entity that absorbed it may still need them.
    pub fn forget(&mut self, path: &str) {
        self.by_path.remove(path);
        self.input_entities.remove(path);
        self.staged_entities.remove(path);
    }

    /// Follow a merge: every promotion pointing at `from` now points at `into`.
    ///
    /// Promotions are decided during classify but written by `commit`, and resolve runs
    /// between them - so a target merged away in that window leaves the edge naming an entity
    /// that no longer exists. Nothing downstream can catch it: `to_entity_path` is a plain string,
    /// and the commit writes it without a lookup.
    ///
    /// [`forget`](Self::forget) is not enough, and the two are not alternatives: it removes
    /// the *merged entity's own* proposal, while this repairs what every **other** entity said
    /// about it. Minting is what made this common rather than theoretical - an entity created
    /// this pass is a fresh identity candidate by construction, so it is far likelier to be
    /// merged than a target that has survived earlier passes.
    ///
    /// A promotion that would now point at its own entity is dropped: the merge can make the
    /// subject and the target the same entity, and an entity does not relate to itself.
    pub fn retarget(&mut self, from: &str, into: &str) {
        if let Some(from_entity) = self.input_entities.get(from).cloned()
            && let Some(into_page) = self.input_entities.get_mut(into)
        {
            if let Some(into_shape) = self.entity_shapes.get_mut(into) {
                if from_entity.name != into_shape.name
                    && !into_shape.aliases.contains(&from_entity.name)
                {
                    into_shape.aliases.push(from_entity.name.clone());
                }
                for alias in &from_entity.aliases {
                    if !into_shape.aliases.contains(alias) {
                        into_shape.aliases.push(alias.clone());
                    }
                }
            }
            if from_entity.name != into_page.name {
                into_page.aliases.insert(from_entity.name.clone());
            }
            into_page.aliases.extend(from_entity.aliases);
            for memory_id in from_entity.source_memory_ids {
                if !into_page.source_memory_ids.contains(&memory_id) {
                    into_page.source_memory_ids.push(memory_id);
                }
            }
            into_page.contributions.extend(from_entity.contributions);
            into_page
                .identity_evidence
                .extend(from_entity.identity_evidence);
            for kind in from_entity.kinds {
                if !into_page.kinds.contains(&kind) {
                    into_page.kinds.push(kind);
                }
            }
            if into_page.description.is_empty() {
                into_page.description = from_entity.description;
            }
            if into_page.body.is_empty() {
                into_page.body = from_entity.body;
            }
            for playbook in from_entity.related_playbooks {
                if !into_page.related_playbooks.contains(&playbook) {
                    into_page.related_playbooks.push(playbook);
                }
            }
            if let (Some(from_attributes), Some(into_attributes)) = (
                from_entity.attributes.as_object(),
                into_page.attributes.as_object_mut(),
            ) {
                for (property, value) in from_attributes {
                    match into_attributes.get_mut(property) {
                        Some(held) => {
                            crate::model::merge_consolidation_attribute_values(held, value)
                        }
                        None => {
                            into_attributes.insert(property.clone(), value.clone());
                        }
                    }
                }
            }
            for source in from_entity.attribute_sources {
                if !into_page.attribute_sources.contains(&source) {
                    into_page.attribute_sources.push(source);
                }
            }
            into_page.outgoing_links.extend(from_entity.outgoing_links);
            into_page.rederive_search();
        }
        if let Some(from_proposal) = self.by_path.remove(from) {
            if let Some(into_proposal) = self.by_path.get_mut(into) {
                for class in from_proposal.classes {
                    if !into_proposal.classes.contains(&class) {
                        into_proposal.classes.push(class);
                    }
                }
                for edit in from_proposal.edits {
                    if !into_proposal.edits.contains(&edit) {
                        into_proposal.edits.push(edit);
                    }
                }
                for rekey in from_proposal.rekeys {
                    if !into_proposal.rekeys.contains(&rekey) {
                        into_proposal.rekeys.push(rekey);
                    }
                }
                for rekey in from_proposal.attr_rekeys {
                    if !into_proposal.attr_rekeys.contains(&rekey) {
                        into_proposal.attr_rekeys.push(rekey);
                    }
                }
                for promotion in from_proposal.promoted {
                    if !into_proposal.promoted.contains(&promotion) {
                        into_proposal.promoted.push(promotion);
                    }
                }
                for (edge, sources) in from_proposal.promoted_sources {
                    let held = into_proposal.promoted_sources.entry(edge).or_default();
                    for source in sources {
                        if !held.contains(&source) {
                            held.push(source);
                        }
                    }
                }
                for retraction in from_proposal.retracted {
                    if !into_proposal.retracted.contains(&retraction) {
                        into_proposal.retracted.push(retraction);
                    }
                }
                for marker in from_proposal.has_keys {
                    if !into_proposal.has_keys.contains(&marker) {
                        into_proposal.has_keys.push(marker);
                    }
                }
                for property in from_proposal.inverse_functional_properties {
                    if !into_proposal
                        .inverse_functional_properties
                        .contains(&property)
                    {
                        into_proposal.inverse_functional_properties.push(property);
                    }
                }
            } else {
                self.by_path.insert(into.to_string(), from_proposal);
            }
        }
        if let Some(from_shape) = self.entity_shapes.remove(from) {
            if let Some(into_shape) = self.entity_shapes.get_mut(into) {
                if from_shape.name != into_shape.name
                    && !into_shape.aliases.contains(&from_shape.name)
                {
                    into_shape.aliases.push(from_shape.name);
                }
                for alias in from_shape.aliases {
                    if !into_shape.aliases.contains(&alias) {
                        into_shape.aliases.push(alias);
                    }
                }
            } else {
                let canonical = self.input_entities.get(into);
                let name = canonical
                    .map(|entity| entity.name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| from_shape.name.clone());
                let description = canonical
                    .map(|entity| entity.description.clone())
                    .filter(|description| !description.is_empty())
                    .unwrap_or(from_shape.description);
                let mut aliases = canonical
                    .map(|entity| entity.aliases.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                if from_shape.name != name && !aliases.contains(&from_shape.name) {
                    aliases.push(from_shape.name);
                }
                for alias in from_shape.aliases {
                    if !aliases.contains(&alias) {
                        aliases.push(alias);
                    }
                }
                self.entity_shapes.insert(
                    into.to_string(),
                    EntityShape {
                        name,
                        description,
                        aliases,
                    },
                );
            }
        }
        self.reconciled_paths.remove(from);
        self.reconciled_paths.remove(into);
        self.staged_entities.remove(from);
        if let Some(canonical) = self.input_entities.get(into)
            && canonical.entity_id.is_none()
            && !canonical.source_memory_ids.is_empty()
        {
            self.staged_entities
                .insert(into.to_string(), canonical.clone());
        }
        for (path, p) in self.by_path.iter_mut() {
            let mut retargeted_sources = HashMap::new();
            for ((property, target), sources) in std::mem::take(&mut p.promoted_sources) {
                let target = if target == from {
                    into.to_string()
                } else {
                    target
                };
                if target != *path {
                    retargeted_sources.insert((property, target), sources);
                }
            }
            p.promoted_sources = retargeted_sources;
            p.promoted.retain_mut(|(_, _, target)| {
                if target == from {
                    *target = into.to_string();
                }
                target != path
            });
        }
    }

    /// The class in force for an entity: what this pass proposed, else its stored kind.
    /// The classes in force for an entity: this pass's proposal if it classified the
    /// entity, else whatever is stored. A proposal is one class - the stage puts a single
    /// type forward at a time - so it supersedes for the purposes of *this* pass
    /// without implying the stored ones go away; stamping adds it alongside them.
    pub fn kinds_for(&self, path: &str, stored: &[String]) -> Vec<String> {
        let mut out = stored.to_vec();
        if let Some(p) = self.by_path.get(path) {
            // Union, not replacement: stamping *adds* a class, so the entity as it will
            // exist carries both what it had and what this pass proposes. Replacing
            // would judge the entity against a version of itself that never exists.
            for c in &p.classes {
                if !out.contains(c) {
                    out.push(c.clone());
                }
            }
        }
        out
    }
}
