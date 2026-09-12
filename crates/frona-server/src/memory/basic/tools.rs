use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::inference::ModelGroup;
use crate::memory::basic::BasicMemoryService;
use frona_derive::agent_tool;

use crate::tool::{InferenceContext, ToolOutput, active_chat, str_arg};

pub struct StoreAgentMemoryTool {
    memory_service: BasicMemoryService,
    compaction_group: Option<ModelGroup>,
    prompts: PromptLoader,
}

impl StoreAgentMemoryTool {
    pub fn new(
        memory_service: BasicMemoryService,
        compaction_group: Option<ModelGroup>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            memory_service,
            compaction_group,
            prompts,
        }
    }
}

#[agent_tool(name = "store_agent_memory")]
impl StoreAgentMemoryTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let memory = str_arg(&arguments, "memory")
            .ok_or_else(|| AppError::Validation("Missing 'memory' parameter".into()))?;

        let overrides = arguments
            .get("overrides")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let agent_id = &ctx.agent.id;
        let chat = active_chat(ctx)?;
        let chat_id = &chat.id;

        tracing::debug!(
            agent_id = %agent_id,
            memory = %memory,
            overrides = overrides,
            "store_agent_memory tool called"
        );

        self.memory_service
            .store_memory_entry(agent_id, memory, Some(chat_id))
            .await?;

        if let Some(ref group) = self.compaction_group {
            let ms = self.memory_service.clone();
            let aid = agent_id.clone();
            let uid = ctx.user.id.clone();
            let group = group.clone();
            if overrides {
                tracing::debug!(agent_id = %aid, "Spawning forced memory compaction (overrides=true)");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_entries_forced(&uid, &aid, &group).await {
                        tracing::warn!(error = %e, agent_id = %aid, "Background forced memory compaction failed");
                    }
                });
            } else {
                tracing::debug!(agent_id = %aid, "Spawning background memory compaction");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_entries_if_needed(&uid, &aid, &group).await {
                        tracing::warn!(error = %e, agent_id = %aid, "Background memory compaction failed");
                    }
                });
            }
        }

        Ok(ToolOutput::text(format!("Stored: {memory}")))
    }
}

pub struct StoreUserMemoryTool {
    memory_service: BasicMemoryService,
    compaction_group: Option<ModelGroup>,
    prompts: PromptLoader,
}

impl StoreUserMemoryTool {
    pub fn new(
        memory_service: BasicMemoryService,
        compaction_group: Option<ModelGroup>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            memory_service,
            compaction_group,
            prompts,
        }
    }
}

#[agent_tool]
impl StoreUserMemoryTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let memory = str_arg(&arguments, "memory")
            .ok_or_else(|| AppError::Validation("Missing 'memory' parameter".into()))?;

        let overrides = arguments
            .get("overrides")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let user_id = &ctx.user.id;
        let chat = active_chat(ctx)?;
        let chat_id = &chat.id;

        tracing::debug!(
            user_id = %user_id,
            memory = %memory,
            overrides = overrides,
            "store_user_memory tool called"
        );

        self.memory_service
            .store_user_memory_entry(user_id, memory, Some(chat_id))
            .await?;

        if let Some(ref group) = self.compaction_group {
            let ms = self.memory_service.clone();
            let uid = user_id.clone();
            let group = group.clone();
            if overrides {
                tracing::debug!(user_id = %uid, "Spawning forced user memory compaction (overrides=true)");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_user_entries_forced(&uid, &group).await {
                        tracing::warn!(error = %e, user_id = %uid, "Background forced user memory compaction failed");
                    }
                });
            } else {
                tracing::debug!(user_id = %uid, "Spawning background user memory compaction");
                tokio::spawn(async move {
                    if let Err(e) = ms.compact_user_entries_if_needed(&uid, &group).await {
                        tracing::warn!(error = %e, user_id = %uid, "Background user memory compaction failed");
                    }
                });
            }
        }

        Ok(ToolOutput::text(format!("Stored for user: {memory}")))
    }
}
