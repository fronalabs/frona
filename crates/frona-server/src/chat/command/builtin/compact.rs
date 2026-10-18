use async_trait::async_trait;

use crate::core::error::AppError;

use super::super::{Command, CommandContext, CommandOutcome};

pub struct CompactCommand;

#[async_trait]
impl Command for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }

    fn description(&self) -> &str {
        "Compress older messages into a summary to free up context."
    }

    async fn run(
        &self,
        _args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandOutcome, AppError> {
        if ctx.session.rig_history != ctx.session.model_history.history {
            return Err(AppError::Conflict(
                "Session history no longer matches its compaction source".into(),
            ));
        }
        let tools = crate::inference::tool_loop::to_rig_tool_definitions(
            ctx.session.tool_registry.definitions(),
            ctx.session.tool_registry.mcp_bridge_mode(),
        );
        let outcome = ctx
            .harness
            .chat_service
            .compactor()
            .compact_chat(
                &ctx.user.id,
                &ctx.chat.id,
                &ctx.chat.agent_id,
                crate::chat::compactor::CompactionContext {
                    system_prompt: &ctx.session.system_prompt,
                    model_group: &ctx.session.model_group,
                    model_history: &ctx.session.model_history,
                    tools: &tools,
                },
            )
            .await?;
        ctx.session.rig_history = outcome.model_history.history.clone();
        ctx.session.model_history = outcome.model_history;
        let status = if outcome.compacted {
            "Compacted older messages into a summary."
        } else {
            "Chat is already at optimal size — no compaction needed."
        };
        Ok(CommandOutcome::Message(status.to_string()))
    }
}
