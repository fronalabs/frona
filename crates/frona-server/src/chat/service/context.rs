use super::*;
use crate::agent::context::{
    AgentContextManager, AgentContextScope, AgentContextSignature, PreparedAgentContext,
};
use crate::chat::message::models::MessageCommand;
use crate::chat::repository::ChatSummaryRepository;

impl ChatService {
    pub fn agent_context_manager(&self) -> &AgentContextManager {
        &self.agent_context_manager
    }

    /// Save only a complete prompt. Retries before this write may rerun the command.
    pub async fn finalize_command_prompt(
        &self,
        message: &Message,
        rendered: &str,
        scope: &crate::agent::context::AgentContextScope<'_>,
    ) -> Result<Message, AppError> {
        if message
            .processed_content
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            return Ok(message.clone());
        }
        let position = self
            .message_repo
            .context_position_before(&message.chat_id, Some(message))
            .await?;
        let summary = crate::db::repo::chat_summaries::SurrealChatSummaryRepo::new(
            self.message_repo.db().clone(),
        )
        .find_by_chat_id(&message.chat_id)
        .await?;
        let previous = effective_signature(&position, summary.as_ref());
        let mut chat = self
            .chat_repo
            .find_by_id(&message.chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat".into()))?;
        let previous_state = chat.agent_context_state.clone();
        let prepared = self
            .agent_context_manager
            .prepare(scope, previous.as_ref(), &mut chat.agent_context_state)
            .await?;
        let mut finalized = message.clone();
        finalized.agent_context_signature = Some(prepared.snapshot.signature);
        finalized.processed_content = Some(format!(
            "{}\n\n{rendered}",
            prepared.update.unwrap_or_default()
        ));
        let saved = self.save_updated_message(&finalized).await?;
        if chat.agent_context_state != previous_state {
            self.chat_repo
                .save_agent_context_state(&chat.id, &chat.agent_context_state)
                .await
                .map_err(crate::inference::controls::required_write_error)?;
        }
        Ok(saved)
    }

    async fn freeze_skill(
        &self,
        user: &crate::auth::User,
        agent: &crate::agent::models::Agent,
        message: &mut Message,
    ) -> Result<(), AppError> {
        if message.processed_content.is_some() {
            return Ok(());
        }
        let Some(MessageCommand::Skill { name, prompt }) = &message.command else {
            return Ok(());
        };
        message.processed_content = self
            .agent_prompt_builder
            .render_skill(user, agent, name, prompt)
            .await?
            .or_else(|| Some(message.content.clone()));
        Ok(())
    }

    /// Collect using the caller's resolved objects and freeze skill expansion.
    pub(super) async fn prepare_message_context(
        &self,
        chat: &mut Chat,
        message: &mut Message,
        scope: &AgentContextScope<'_>,
        previous: Option<&AgentContextSignature>,
    ) -> Result<PreparedAgentContext, AppError> {
        let prepared = self
            .agent_context_manager
            .prepare(scope, previous, &mut chat.agent_context_state)
            .await?;
        self.freeze_skill(scope.user, scope.agent, message).await?;
        Ok(prepared)
    }
}

pub(super) fn effective_signature(
    position: &crate::db::repo::message_context::MessageContextPosition,
    summary: Option<&crate::chat::models::ChatSummary>,
) -> Option<AgentContextSignature> {
    // Late inherited assistant rows must not rewind the comparison position.
    position
        .latest_entry
        .as_ref()
        .filter(|row| summary.is_none_or(|summary| row.created_at > summary.updated_at))
        .and_then(|row| row.agent_context_signature.clone())
        .or_else(|| summary.and_then(|s| s.agent_context_signature.clone()))
}
