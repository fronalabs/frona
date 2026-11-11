use crate::{
    classify::{OntologyDeclaration, ProposalSet},
    model::{AttributeSource, RelationType},
};
use std::collections::HashSet;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReconcilePromotion {
    pub key: String,
    pub property: String,
    pub target: String,
    pub source_memory_ids: Vec<String>,
    pub declaration: Option<serde_json::Value>,
}

/// Accepted reconciliation, still uncommitted. The host applies memory lifecycle
/// changes and checkpoint advancement atomically, then requeues `reconcile_dirty`.
/// When memories are retired, the entity summary is deliberately deferred until
/// a new invocation sees the updated memory set.
#[derive(Debug, Default, Clone)]
pub struct ReconcileOutput {
    pub reconcile_dirty: HashSet<String>,
    pub promotions: Vec<ReconcilePromotion>,
    pub retractions: Vec<(String, String)>,
    pub entity_update: Option<ReconciledEntityUpdate>,
    pub changes: ReconcileChanges,
    /// Raw self-entity attributes. The host applies its account field allowlist;
    /// this crate never reads or writes a User record.
    pub profile_attributes: Option<serde_json::Value>,
    /// A requested move checked against the supplied snapshot. The host must
    /// recheck destination availability when committing it.
    pub move_entity: Option<EntityMove>,
    pub proposals: ProposalSet,
}

#[derive(Debug, Clone)]
pub struct EntityMove {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone)]
pub struct ReconciledEntityUpdate {
    pub name: String,
    pub description: String,
    pub attributes: serde_json::Value,
    pub attribute_sources: Vec<AttributeSource>,
    pub declarations: Vec<OntologyDeclaration>,
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileChanges {
    pub memory_relations: Vec<MemoryRelationChange>,
    pub outdated_memories: Vec<OutdatedMemory>,
    pub entity_link_sources: Vec<EntityLinkSources>,
}

#[derive(Debug, Clone)]
pub struct MemoryRelationChange {
    pub subordinate_id: String,
    pub relation: RelationType,
    pub to_id: String,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct OutdatedMemory {
    pub memory_id: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct EntityLinkSources {
    pub from_entity_path: String,
    pub to_entity_path: String,
    pub relation: String,
    pub source_memory_ids: Vec<String>,
}
