use async_trait::async_trait;
use serde_json::Value;

use crate::error::AppError;
use crate::llm::config::ModelGroup;
use crate::memory::service::MemoryService;

use super::{AgentTool, ToolDefinition, ToolOutput};

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
        "remember"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "remember_fact".to_string(),
            description: "Store an important fact about the user for long-term memory. \
Before calling this tool, check <agent_memory> to avoid storing duplicates. \
Each fact should be a short, atomic statement about the USER — their preferences, personal details, \
project context, or decisions. Do NOT store your own responses, observations, or trivial conversation. \
Set overrides to true when the new fact contradicts or updates a previously stored fact.".to_string(),
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

    async fn execute(&self, _tool_name: &str, arguments: Value) -> Result<ToolOutput, AppError> {
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
            "remember_fact tool called"
        );

        self.memory_service
            .store_fact(&self.agent_id, fact, Some(&self.chat_id))
            .await?;

        if let Some(ref group) = self.compaction_group {
            let ms = self.memory_service.clone();
            let aid = self.agent_id.clone();
            let group = group.clone();
            if overrides {
                tracing::debug!(agent_id = %self.agent_id, "Spawning forced fact compaction (overrides=true)");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_facts_forced(&aid, &group).await {
                        tracing::warn!(error = %e, agent_id = %aid, "Background forced fact compaction failed");
                    }
                });
            } else {
                tracing::debug!(agent_id = %self.agent_id, "Spawning background fact compaction");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_facts_if_needed(&aid, &group).await {
                        tracing::warn!(error = %e, agent_id = %aid, "Background fact compaction failed");
                    }
                });
            }
        }

        Ok(ToolOutput::text(format!("Remembered: {fact}")))
    }
}
