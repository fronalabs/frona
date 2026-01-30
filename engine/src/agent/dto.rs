use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::models::SandboxSettings;
use crate::tool::configurable_tools;

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub model_group: Option<String>,
    pub tools: Option<Vec<String>>,
    pub sandbox_config: Option<SandboxSettings>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
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
    pub system_prompt: String,
    pub model_group: String,
    pub enabled: bool,
    pub tools: Vec<String>,
    pub sandbox_config: Option<SandboxSettings>,
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

impl From<super::models::Agent> for AgentResponse {
    fn from(agent: super::models::Agent) -> Self {
        Self {
            id: agent.id,
            name: agent.name,
            description: agent.description,
            system_prompt: agent.system_prompt,
            model_group: agent.model_group,
            enabled: agent.enabled,
            tools: normalize_tools(agent.tools),
            sandbox_config: agent.sandbox_config,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        }
    }
}
