use crate::prompt::PromptIds;
use crate::{Error, classify::OntologyDeclaration, model::RelationType};
use serde::Deserialize;
/// The model's verdict for one entity.
#[derive(Debug, Default, Clone, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub(super) struct EntityVerdict {
    pub(super) relations: Vec<Related>,
    /// Entity-to-entity facts. Distinct from `relations`, which relates memory records.
    pub(super) entity_relations: Vec<EntityRelation>,
    /// Explicit removals of asserted entity links whose supporting memories are retired.
    pub(super) relation_retractions: Vec<EntityRetraction>,
    /// Explicit object-property target changes on this entity.
    pub(super) entity_relation_replacements: Vec<EntityRelationReplacement>,
    pub(super) outdated: Vec<Outdated>,
    pub(super) attributes: serde_json::Value,
    /// Database-only provenance for every scalar value / array member in `attributes`.
    pub(super) attribute_sources: Vec<AttributeSourceInput>,
    /// Explicit data-property value changes on this entity.
    pub(super) attribute_replacements: Vec<AttributeReplacement>,
    /// A better display title for the entity, or blank to keep the current one.
    /// Distinct from `moves`, which changes where the entity *lives*.
    pub(super) name: String,
    pub(super) description: String,
    pub(super) moves: Vec<Move>,
    pub(super) declarations: Vec<OntologyDeclaration>,
}

impl EntityVerdict {
    pub(super) fn expand_prompt_ids(&mut self, ids: &PromptIds) -> Result<(), Error> {
        for related in &mut self.relations {
            related.memory = ids.expand(&related.memory)?;
            for link in &mut related.links {
                link.to = ids.expand(&link.to)?;
            }
        }
        for outdated in &mut self.outdated {
            outdated.memory = ids.expand(&outdated.memory)?;
        }
        for source in &mut self.attribute_sources {
            ids.expand_all(&mut source.source_memory_ids)?;
        }
        for relation in &mut self.entity_relations {
            ids.expand_all(&mut relation.source_memory_ids)?;
        }
        for replacement in &mut self.entity_relation_replacements {
            ids.expand_all(&mut replacement.old_source_memory_ids)?;
            ids.expand_all(&mut replacement.new_source_memory_ids)?;
        }
        for replacement in &mut self.attribute_replacements {
            ids.expand_all(&mut replacement.old_source_memory_ids)?;
            ids.expand_all(&mut replacement.new_source_memory_ids)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct EntityRelation {
    /// The key from `attributes` whose value names the target entity.
    pub(super) attribute: String,
    /// The exact attribute value being interpreted as an entity.
    pub(super) value: String,
    /// Object-property CURIE chosen for this entity-to-entity fact.
    #[serde(default)]
    pub(super) property: String,
    /// An existing entity path offered by the advisory check.
    pub(super) target: String,
    #[serde(default)]
    pub(super) source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct EntityRetraction {
    pub(super) property: String,
    pub(super) target: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct EntityRelationReplacement {
    pub(super) property: String,
    pub(super) was_target: String,
    pub(super) now_target: String,
    #[serde(default)]
    pub(super) old_source_memory_ids: Vec<String>,
    #[serde(default)]
    pub(super) new_source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct AttributeSourceInput {
    pub(super) property: String,
    pub(super) value: serde_json::Value,
    #[serde(default)]
    pub(super) source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct AttributeReplacement {
    pub(super) property: String,
    pub(super) was: serde_json::Value,
    pub(super) now: serde_json::Value,
    #[serde(default)]
    pub(super) old_source_memory_ids: Vec<String>,
    #[serde(default)]
    pub(super) new_source_memory_ids: Vec<String>,
}

#[derive(Debug)]
pub(super) struct PromotionSuggestion {
    pub(super) attribute: String,
    pub(super) value: String,
    pub(super) candidates: Vec<(String, String)>,
}

/// One subordinate memory + its typed relations to survivors.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct Related {
    pub(super) memory: String,
    #[serde(default)]
    pub(super) links: Vec<RelationInput>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct RelationInput {
    pub(super) relation: RelationType,
    #[serde(default)]
    pub(super) to: String,
    /// `replace` only: the value that changed, verbatim from the **older** entry.
    #[serde(default)]
    pub(super) was: String,
    /// `replace` only: what it changed to, verbatim from the **newer** entry.
    #[serde(default)]
    pub(super) now: String,
    #[serde(default)]
    pub(super) note: String,
    /// Internal marker: only deterministic typed-property closure can set this.
    #[serde(skip)]
    #[schemars(skip)]
    pub(super) derived_from_property: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct Outdated {
    pub(super) memory: String,
    #[serde(default)]
    pub(super) note: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(super) struct Move {
    pub(super) from: String,
    pub(super) to: String,
}
