use async_trait::async_trait;
use serde_json::Value;

use crate::core::error::AppError;
use crate::inference::config::ModelGroup;
use crate::memory::service::MemoryService;

use super::{AgentTool, ToolContext, ToolDefinition, ToolOutput};

pub struct RememberTool {
    memory_service: MemoryService,
    agent_id: String,
    chat_id: String,
    compaction_group: Option<ModelGroup>,
}

impl RememberTool {
    pub fn new(
        memory_service: MemoryService,
        agent_id: String,
        chat_id: String,
        compaction_group: Option<ModelGroup>,
    ) -> Self {
        Self {
            memory_service,
            agent_id,
            chat_id,
            compaction_group,
        }
    }
}

#[async_trait]
impl AgentTool for RememberTool {
    fn name(&self) -> &str {
        "remember_agent_fact"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "remember_agent_fact".to_string(),
            description: "Store an insight for this agent's long-term memory. \
IMPORTANT: Before calling, carefully review <agent_memory>. Do NOT call this tool if the insight — \
or something very similar — is already listed there, even if worded differently. \
Each insight should be a short, atomic statement — working context, project details, \
decisions, or anything relevant to this agent's work. \
Set overrides to true ONLY when the new insight contradicts or updates a previously stored one.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "fact": {
                        "type": "string",
                        "description": "A short, atomic fact about the user to remember"
                    },
                    "overrides": {
                        "type": "boolean",
                        "description": "Set to true if this fact contradicts or supersedes a previously stored fact",
                        "default": false
                    }
                },
                "required": ["fact"]
            }),
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let fact = arguments
            .get("fact")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'fact' parameter".into()))?;

        let overrides = arguments
            .get("overrides")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        tracing::debug!(
            agent_id = %self.agent_id,
            fact = %fact,
            overrides = overrides,
            "remember tool called"
        );

        self.memory_service
            .store_insight(&self.agent_id, fact, Some(&self.chat_id))
            .await?;

        if let Some(ref group) = self.compaction_group {
            let ms = self.memory_service.clone();
            let aid = self.agent_id.clone();
            let group = group.clone();
            if overrides {
                tracing::debug!(agent_id = %self.agent_id, "Spawning forced insight compaction (overrides=true)");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_insights_forced(&aid, &group).await {
                        tracing::warn!(error = %e, agent_id = %aid, "Background forced insight compaction failed");
                    }
                });
            } else {
                tracing::debug!(agent_id = %self.agent_id, "Spawning background insight compaction");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_insights_if_needed(&aid, &group).await {
                        tracing::warn!(error = %e, agent_id = %aid, "Background insight compaction failed");
                    }
                });
            }
        }

        Ok(ToolOutput::text(format!("Remembered: {fact}")))
    }
}

pub struct RememberUserFactTool {
    memory_service: MemoryService,
    user_id: String,
    chat_id: String,
    compaction_group: Option<ModelGroup>,
}

impl RememberUserFactTool {
    pub fn new(
        memory_service: MemoryService,
        user_id: String,
        chat_id: String,
        compaction_group: Option<ModelGroup>,
    ) -> Self {
        Self {
            memory_service,
            user_id,
            chat_id,
            compaction_group,
        }
    }
}

#[async_trait]
impl AgentTool for RememberUserFactTool {
    fn name(&self) -> &str {
        "remember_user_fact"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "remember_user_fact".to_string(),
            description: "Store a fact about the user that persists across ALL agents. \
Call this whenever the user shares something genuinely new about themselves — \
name, location, job, hobbies, preferences, relationships, goals, opinions. \
IMPORTANT: Before calling, carefully review <user_memory>. Do NOT call this tool if the fact — \
or something very similar — is already listed there, even if worded differently. \
Only call when you have genuinely new information. \
Set overrides to true ONLY when the new fact contradicts or updates a previously stored one.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "fact": {
                        "type": "string",
                        "description": "A short, atomic fact about the user to remember across all agents"
                    },
                    "overrides": {
                        "type": "boolean",
                        "description": "Set to true if this fact contradicts or supersedes a previously stored fact",
                        "default": false
                    }
                },
                "required": ["fact"]
            }),
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let fact = arguments
            .get("fact")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'fact' parameter".into()))?;

        let overrides = arguments
            .get("overrides")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        tracing::debug!(
            user_id = %self.user_id,
            fact = %fact,
            overrides = overrides,
            "remember_user_fact tool called"
        );

        self.memory_service
            .store_user_insight(&self.user_id, fact, Some(&self.chat_id))
            .await?;

        if let Some(ref group) = self.compaction_group {
            let ms = self.memory_service.clone();
            let uid = self.user_id.clone();
            let group = group.clone();
            if overrides {
                tracing::debug!(user_id = %self.user_id, "Spawning forced user insight compaction (overrides=true)");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_user_insights_forced(&uid, &group).await {
                        tracing::warn!(error = %e, user_id = %uid, "Background forced user insight compaction failed");
                    }
                });
            } else {
                tracing::debug!(user_id = %self.user_id, "Spawning background user insight compaction");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_user_insights_if_needed(&uid, &group).await {
                        tracing::warn!(error = %e, user_id = %uid, "Background user insight compaction failed");
                    }
                });
            }
        }

        Ok(ToolOutput::text(format!("Remembered for user: {fact}")))
    }
}
