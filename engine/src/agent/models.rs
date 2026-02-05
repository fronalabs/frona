use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use crate::Entity;
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::{deserialize_bool_from_anything, deserialize_number_from_string};
use surrealdb::types::SurrealValue;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct SandboxSettings {
    #[serde(default = "serde_aux::field_attributes::bool_true", deserialize_with = "deserialize_bool_from_anything")]
    pub network_access: bool,
    #[serde(default)]
    pub allowed_network_destinations: Vec<String>,
    #[serde(default = "serde_aux::field_attributes::default_u64::<30>", deserialize_with = "deserialize_number_from_string")]
    pub timeout_secs: u64,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            network_access: true,
            allowed_network_destinations: Vec::new(),
            timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "agent")]
pub struct Agent {
    pub id: String,
    #[serde(default)]
    pub user_id: Option<String>,
    pub name: String,
    pub description: String,
    pub model_group: String,
    pub enabled: bool,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub sandbox_config: Option<SandboxSettings>,
    #[serde(default)]
    pub max_concurrent_tasks: Option<u32>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub identity: BTreeMap<String, String>,
    #[serde(default)]
    pub heartbeat_interval: Option<u64>,
    pub next_heartbeat_at: Option<DateTime<Utc>>,
    pub heartbeat_chat_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
