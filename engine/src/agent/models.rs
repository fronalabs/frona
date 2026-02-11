use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use crate::Entity;
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::{deserialize_bool_from_anything, deserialize_number_from_string};
use surrealdb::types::SurrealValue;

use crate::tool::configurable_tools;

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

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub name: String,
    pub description: String,
    pub model_group: Option<String>,
    pub tools: Option<Vec<String>>,
    pub sandbox_config: Option<SandboxSettings>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub model_group: Option<String>,
    pub enabled: Option<bool>,
    pub tools: Option<Vec<String>>,
    pub sandbox_config: Option<SandboxSettings>,
}

#[derive(Debug, Serialize)]
pub struct AgentResponse {
    pub id: String,
    pub name: String,
    pub description: String,
    pub model_group: String,
    pub enabled: bool,
    pub tools: Vec<String>,
    pub sandbox_config: Option<SandboxSettings>,
    pub avatar: Option<String>,
    pub identity: BTreeMap<String, String>,
    pub chat_count: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn normalize_tools(tools: Vec<String>) -> Vec<String> {
    if tools.is_empty() {
        configurable_tools().to_vec()
    } else {
        tools
    }
}

impl From<Agent> for AgentResponse {
    fn from(agent: Agent) -> Self {
        Self {
            id: agent.id,
            name: agent.name,
            description: agent.description,
            model_group: agent.model_group,
            enabled: agent.enabled,
            tools: normalize_tools(agent.tools),
            sandbox_config: agent.sandbox_config,
            avatar: agent.avatar,
            identity: agent.identity,
            chat_count: 0,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        }
    }
}
