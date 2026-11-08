//! Owned runtime tool preparation for application-managed, in-memory SDK loops.

use super::harness::Harness;
use crate::{
    core::error::AppError,
    inference::request::InferenceContext,
    tool::{
        AgentTool, ToolOutput,
        registry::{AgentToolRegistry, ToolFilter},
    },
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Owns the identity-bound dependencies while a portable registry borrows them.
/// Preparation performs no model/tool invocation and creates no Chat/message rows.
pub struct PreparedAgentTools {
    pub(crate) registry: AgentToolRegistry,
    pub(crate) context: InferenceContext,
}

impl PreparedAgentTools {
    pub fn portable<'a, F>(&'a self, output: F) -> Result<frona_harness::ToolRegistry<'a>, AppError>
    where
        F: FnMut(ToolOutput) -> frona_harness::ToolResult + Clone + Send + 'a,
    {
        self.registry.portable(&self.context, output)
    }
}

impl Harness {
    /// Resolve real runtime identity and policy, then bind explicitly selected tools.
    /// Extra tools retain the existing caller-owned required-tool registration rule.
    pub async fn prepare_agent_tools(
        &self,
        user_id: &str,
        agent_id: &str,
        chat_id: Option<&str>,
        filters: &[ToolFilter],
        extra: &[Arc<dyn AgentTool>],
        cancellation: CancellationToken,
    ) -> Result<PreparedAgentTools, AppError> {
        let agent = self
            .agent_service
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent not found: {agent_id}")))?;
        if agent.user_id != user_id {
            return Err(AppError::Forbidden("Agent belongs to another user".into()));
        }
        let mut registry = self
            .tool_manager
            .build_agent_registry(user_id, &agent, &self.policy_service, None)
            .await;
        for filter in filters {
            registry.apply_filter(filter);
        }
        for tool in extra {
            registry.register_required(tool.clone())?;
        }
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("User not found: {user_id}")))?;
        let context = match chat_id {
            Some(id) => {
                let chat = self.chat_service.get_chat(user_id, id).await?;
                let events =
                    self.broadcast_service
                        .create_event_sender(user_id, id, chat.space_id.clone());
                InferenceContext::new(
                    user,
                    agent,
                    chat,
                    events,
                    self.shutdown_token.clone(),
                    cancellation,
                )
            }
            None => InferenceContext::new_detached(
                user,
                agent,
                self.shutdown_token.clone(),
                cancellation,
            ),
        };
        Ok(PreparedAgentTools { registry, context })
    }
}
