use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

/// Durable serial work queue for Playbook identity and memory ownership.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct PlaybookResolveState {
    pub revision: u64,
}
