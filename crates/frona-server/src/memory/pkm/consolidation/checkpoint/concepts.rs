use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use surrealdb::types::SurrealValue;

/// Global Consolidator checkpoint data. Entity payload and per-entity progress live on
/// `KnowledgeConsolidationEntity` rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ConsolidationWorkState {
    /// Canonically ordered entity pair → the narrow identity evidence most recently
    /// presented to Resolve. It records only names, classes, and shared marker evidence.
    pub resolution_pair_fingerprints: BTreeMap<String, String>,
    /// The batch schema decision, once adjudicate has produced one.
    pub adjudicated: Option<serde_json::Value>,
    /// Per-term decisions accumulated across bounded adjudication batches.
    pub adjudicated_terms: BTreeMap<String, serde_json::Value>,
    /// Normalized vocabulary query → rendered catalogue hits. The catalogue is immutable
    /// for the lifetime of a pass, so every later classify iteration can reuse these.
    pub vocabulary_search_cache: BTreeMap<String, String>,
    /// Normalized entity query → `{ revision, result }`. Entity search is revision-scoped
    /// because an accepted classify/resolve iteration may mint or merge an entity.
    pub entity_search_cache: BTreeMap<String, serde_json::Value>,
    pub revision: u64,
}
