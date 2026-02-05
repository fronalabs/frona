use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::models::SandboxSettings;
use crate::tool::configurable_tools;

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

impl From<super::models::Agent> for AgentResponse {
    fn from(agent: super::models::Agent) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::models::Agent;
    use chrono::Utc;

    #[test]
    fn agent_response_from_agent_defaults_chat_count_to_zero() {
        let now = Utc::now();
        let agent = Agent {
            id: "test".to_string(),
            user_id: Some("u1".to_string()),
            name: "Test".to_string(),
            description: "desc".to_string(),
            model_group: "primary".to_string(),
            enabled: true,
            tools: vec![],
            sandbox_config: None,
            max_concurrent_tasks: None,
            avatar: None,
            identity: BTreeMap::new(),
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        };
        let response = AgentResponse::from(agent);
        assert_eq!(response.chat_count, 0);
    }
}
