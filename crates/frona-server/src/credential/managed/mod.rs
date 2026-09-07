//! Encrypted integration credentials. Ownership is storage isolation, not an
//! authorization grant. Callers must authorize access to the owning vault connection.

pub mod integration;
pub mod login;
pub mod repository;
pub mod resolver;
mod storage;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use storage::ManagedVault;
use uuid::Uuid;

/// The built-in shared vault used by every model provider.
pub const GLOBAL_CONNECTION_ID: &str = "managed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub connection: String,
    pub slot: String,
}

impl Key {
    pub fn new(connection: impl Into<String>, slot: impl Into<String>) -> Self {
        Self {
            connection: connection.into(),
            slot: slot.into(),
        }
    }
}

#[derive(Clone)]
pub enum Candidate {
    Secret(Value),
    Existing(Uuid),
    /// Coordination only, with no secret to activate or export.
    External,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Secret(_) => f.write_str("Secret([REDACTED])"),
            Self::Existing(version) => f.debug_tuple("Existing").field(version).finish(),
            Self::External => f.write_str("External"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub integration: String,
    pub deleted: bool,
    pub key: Key,
    pub item_id: Uuid,
    pub generation: u64,
    pub version: Uuid,
    pub removed: bool,
    pub metadata: Value,
    pub updated_at: DateTime<Utc>,
}
