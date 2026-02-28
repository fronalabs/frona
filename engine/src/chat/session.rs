use rig::completion::Message as RigMessage;
use tokio::sync::mpsc::Sender;
pub use tokio_util::sync::CancellationToken;

use crate::chat::models::Chat;
use crate::chat::service::AgentConfig;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::core::state::AppState;
use crate::inference::config::ModelGroup;
use crate::inference::convert::to_rig_messages;
use crate::inference::tool_loop::InferenceEvent;
use crate::inference::ModelProviderRegistry;
use crate::tool::registry::AgentToolRegistry;
use crate::tool::ToolContext;

pub struct ChatSessionContext {
    pub chat: Chat,
    pub agent_config: AgentConfig,
    pub system_prompt: String,
    pub model_group: ModelGroup,
    pub rig_history: Vec<RigMessage>,
    pub registry: ModelProviderRegistry,
    pub tool_registry: AgentToolRegistry,
    pub tool_ctx: ToolContext,
    pub cancel_token: CancellationToken,
    pub tool_event_tx: Sender<InferenceEvent>,
    pub tool_event_rx: tokio::sync::mpsc::Receiver<InferenceEvent>,
}

impl ChatSessionContext {
    pub async fn build(
        state: &AppState,
        user_id: &str,
        chat: Chat,
        cancel_token: CancellationToken,
        tool_event_tx: Sender<InferenceEvent>,
        tool_event_rx: tokio::sync::mpsc::Receiver<InferenceEvent>,
    ) -> Result<Self, AppError> {
        let agent_config = state
            .chat_service
            .resolve_agent_config(&chat.agent_id)
            .await?;

        let skill_summaries: Vec<(String, String)> = state
            .skill_resolver
            .list(&chat.agent_id)
            .await
            .into_iter()
            .map(|s| (s.name, s.description))
            .collect();

        let agent_summaries =
            crate::api::routes::messages::build_agent_summaries_from_state(
                state,
                user_id,
                &chat.agent_id,
                &agent_config.tools,
            )
            .await;

        let mut system_prompt = match state
            .memory_service
            .build_augmented_system_prompt(
                &agent_config.system_prompt,
                &chat.agent_id,
                user_id,
                chat.space_id.as_deref(),
                &skill_summaries,
                &agent_summaries,
                &agent_config.identity,
            )
            .await
        {
            Ok(prompt) => prompt,
            Err(e) => {
                tracing::warn!(error = %e, agent_id = %chat.agent_id, "Failed to build augmented system prompt, using base");
                agent_config.system_prompt.clone()
            }
        };

        let model_group = state
            .chat_service
            .provider_registry()
            .resolve_model_group(&agent_config.model_group)?;

        let stored_messages = state.chat_service.get_stored_messages(&chat.id).await;

        // Append any per-turn context injected by tools (e.g. active_call block),
        // in the order they appear in the conversation.
        for msg in &stored_messages {
            if let Some(sp) = &msg.system_prompt {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(sp);
            }
        }

        let rig_history = to_rig_messages(&stored_messages, &chat.agent_id);

        let registry = state.chat_service.provider_registry().clone();

        let user = state
            .user_repo
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;

        let tool_registry = crate::api::routes::messages::build_tool_registry(
            state,
            &chat.agent_id,
            user_id,
            &user.username,
            &chat.id,
            &agent_config.tools,
            agent_config.sandbox_config.as_ref(),
        )
        .await;
        let agent = state
            .agent_service
            .find_by_id(&chat.agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;
        let tool_ctx = ToolContext {
            user,
            agent,
            chat: chat.clone(),
            event_tx: tool_event_tx.clone(),
        };

        Ok(Self {
            chat,
            agent_config,
            system_prompt,
            model_group,
            rig_history,
            registry,
            tool_registry,
            tool_ctx,
            cancel_token,
            tool_event_tx,
            tool_event_rx,
        })
    }
}
