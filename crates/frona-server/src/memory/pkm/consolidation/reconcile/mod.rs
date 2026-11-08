//! Reconcile each changed concept entity: record supersessions, refresh
//! attributes/description, apply an optional move, and (for the self-entity) project
//! account-backed attributes onto the `User` record.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;

use crate::core::error::AppError;
use crate::db::repo::pkm::ReconcileCommit;
use crate::memory::pkm::model::{AttributeSource, RelationType};

use crate::memory::pkm::consolidation::PromptIds;
use crate::memory::pkm::consolidation::ReconcilePromotion;
use crate::memory::pkm::consolidation::classify::OntologyDeclaration;
use crate::memory::pkm::consolidation::context::ConsolidationContext;

#[derive(Clone)]
pub(super) struct Reconcile {
    pub ctx: Arc<ConsolidationContext>,
    pub ontology: crate::memory::pkm::ontology::OntologyManager,
    /// Only for the self-entity → `User` write-through. No other stage touches the account.
    pub users: crate::auth::user_service::UserService,
    /// The CURIE bindings in force, from the catalogue that owns them. Carried rather than
    /// rebuilt with `PrefixMap::standard()`: which prefixes are bound is what decides
    /// whether an attribute key is a usable term, and this stage both compares keys against
    /// existing relations and mints new ones.
    pub prefixes: crate::memory::pkm::ontology::PrefixMap,
    pub max_submissions: usize,
}

/// The model's verdict for one entity.
#[derive(Debug, Default, Clone, Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct EntityVerdict {
    relations: Vec<Related>,
    /// Entity-to-entity facts. Distinct from `relations`, which relates memory records.
    entity_relations: Vec<EntityRelation>,
    /// Explicit removals of asserted entity links whose supporting memories are retired.
    relation_retractions: Vec<EntityRetraction>,
    /// Explicit object-property target changes on this entity.
    entity_relation_replacements: Vec<EntityRelationReplacement>,
    outdated: Vec<Outdated>,
    attributes: serde_json::Value,
    /// Database-only provenance for every scalar value / array member in `attributes`.
    attribute_sources: Vec<AttributeSourceInput>,
    /// Explicit data-property value changes on this entity.
    attribute_replacements: Vec<AttributeReplacement>,
    /// A better display title for the entity, or blank to keep the current one.
    /// Distinct from `moves`, which changes where the entity *lives*.
    name: String,
    description: String,
    moves: Vec<Move>,
    declarations: Vec<OntologyDeclaration>,
}

impl EntityVerdict {
    fn expand_prompt_ids(&mut self, ids: &PromptIds) -> Result<(), AppError> {
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
struct EntityRelation {
    /// The key from `attributes` whose value names the target entity.
    attribute: String,
    /// The exact attribute value being interpreted as an entity.
    value: String,
    /// Object-property CURIE chosen for this entity-to-entity fact.
    #[serde(default)]
    property: String,
    /// An existing entity path offered by the advisory check.
    target: String,
    #[serde(default)]
    source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct EntityRetraction {
    property: String,
    target: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct EntityRelationReplacement {
    property: String,
    was_target: String,
    now_target: String,
    #[serde(default)]
    old_source_memory_ids: Vec<String>,
    #[serde(default)]
    new_source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct AttributeSourceInput {
    property: String,
    value: serde_json::Value,
    #[serde(default)]
    source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct AttributeReplacement {
    property: String,
    was: serde_json::Value,
    now: serde_json::Value,
    #[serde(default)]
    old_source_memory_ids: Vec<String>,
    #[serde(default)]
    new_source_memory_ids: Vec<String>,
}

#[derive(Debug)]
struct PromotionSuggestion {
    attribute: String,
    value: String,
    candidates: Vec<(String, String)>,
}

/// One subordinate memory + its typed relations to survivors.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct Related {
    memory: String,
    #[serde(default)]
    links: Vec<RelationInput>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct RelationInput {
    relation: RelationType,
    #[serde(default)]
    to: String,
    /// `replace` only: the value that changed, verbatim from the **older** entry.
    #[serde(default)]
    was: String,
    /// `replace` only: what it changed to, verbatim from the **newer** entry.
    #[serde(default)]
    now: String,
    #[serde(default)]
    note: String,
    /// Internal marker: only deterministic typed-property closure can set this.
    #[serde(skip)]
    #[schemars(skip)]
    derived_from_property: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct Outdated {
    memory: String,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct Move {
    from: String,
    to: String,
}

/// What one entity's reconcile produced: the entities that **gained** a live memory via a
/// entity-link union, which is what re-enters the drain's worklist.
///
/// Entities that merely need re-rendering are not carried - retiring, unioning and
/// quarantining all bump the affected entity's `updated_at`, so author derives them.
#[derive(Default)]
struct EntityOutcome {
    reconcile_dirty: HashSet<String>,
    promotions: Vec<ReconcilePromotion>,
    retractions: Vec<(String, String)>,
    page_update: Option<ReconciledEntityUpdate>,
    commit: ReconcileCommit,
    profile_attributes: Option<serde_json::Value>,
}

struct ReconciledEntityUpdate {
    name: String,
    description: String,
    attributes: serde_json::Value,
    attribute_sources: Vec<AttributeSource>,
    declarations: Vec<OntologyDeclaration>,
}

mod inspection;
mod projection;
mod run;
mod validation;

#[cfg(test)]
mod tests;
