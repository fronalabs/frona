use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::inference::config::ModelGroup;
use crate::memory::service::MemoryService;
use frona_derive::agent_tool;

use super::{ToolContext, ToolOutput};

pub struct RememberTool {
    memory_service: MemoryService,
    agent_id: String,
    chat_id: String,
    compaction_group: Option<ModelGroup>,
    prompts: PromptLoader,
}

impl RememberTool {
    pub fn new(
        memory_service: MemoryService,
        agent_id: String,
        chat_id: String,
        compaction_group: Option<ModelGroup>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            memory_service,
            agent_id,
            chat_id,
            compaction_group,
            prompts,
        }
    }
}

#[agent_tool(name = "remember_agent_fact")]
impl RememberTool {
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
    prompts: PromptLoader,
}

impl RememberUserFactTool {
    pub fn new(
        memory_service: MemoryService,
        user_id: String,
        chat_id: String,
        compaction_group: Option<ModelGroup>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            memory_service,
            user_id,
            chat_id,
            compaction_group,
            prompts,
        }
    }
}

#[agent_tool]
impl RememberUserFactTool {
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
