//! Per-turn override only — the chat's persistent `agent_id` is NOT mutated.
//! The next turn fires with the chat's default agent again.

use async_trait::async_trait;

use crate::chat::message::models::MessageCommand;
use crate::chat::session::ChatSessionContext;
use crate::core::error::AppError;

use super::super::{Command, CommandContext, CommandOutcome};

pub struct SwitchAgentCommand;

#[async_trait]
impl Command for SwitchAgentCommand {
    fn name(&self) -> &str {
        // Registered as the agent-handle fallback, never under this name.
        "agent"
    }

    fn description(&self) -> &str {
        "Delegate this single message to a different agent."
    }

    fn argument_hint(&self) -> Option<&str> {
        Some("[prompt]")
    }

    async fn run(
        &self,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<CommandOutcome, AppError> {
        // One handler serves every agent; the name lives on the message.
        let agent_handle = match &ctx.request.command {
            Some(MessageCommand::Command { name, .. }) => name.clone(),
            _ => {
                return Err(AppError::Internal(
                    "SwitchAgentCommand invoked without a Command invocation on the message".into(),
                ));
            }
        };

        let target_agent = ctx
            .harness
            .agent_service
            .find_by_handle(&ctx.user.id, &agent_handle)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("Agent '{agent_handle}' not found for user"))
            })?;

        let mut chat_for_turn = ctx.chat.clone();
        chat_for_turn.agent_id = target_agent.id.clone();
        let new_session = ChatSessionContext::build(
            ctx.harness,
            &ctx.user.id,
            chat_for_turn,
            ctx.session.cancel_token.clone(),
            ctx.session.builder.clone(),
            &ctx.session.tool_filters,
        )
        .await?;
        *ctx.session = new_session;
        ctx.response.agent_id = Some(target_agent.id.clone());

        Ok(CommandOutcome::Prompt(args.to_string()))
    }
}
