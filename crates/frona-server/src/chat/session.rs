use std::sync::Arc;

use rig_core::completion::Message as RigMessage;
pub use tokio_util::sync::CancellationToken;

use crate::agent::harness::Harness;
use crate::chat::broadcast::EventSender;
use crate::chat::models::Chat;
use crate::chat::service::AgentConfig;
use crate::core::error::AppError;
use crate::inference::conversation::{
    ConversationBuilder, ConversationContext, ModelConversationHistory, resolve_attachment_path,
};
use crate::tool::InferenceContext;
use crate::tool::registry::{AgentToolRegistry, ToolFilter};
use frona_harness::inference::ModelGroup;

pub struct ChatSessionContext {
    pub chat: Chat,
    pub agent_config: AgentConfig,
    pub system_prompt: String,
    pub model_group: ModelGroup,
    pub rig_history: Vec<RigMessage>,
    pub(crate) model_history: ModelConversationHistory,
    pub(crate) input_token_estimate: Option<frona_harness::inference::context::InputTokenEstimate>,
    pub(crate) builder: Arc<dyn ConversationBuilder>,
    pub(crate) tool_filters: Vec<ToolFilter>,

    pub tool_registry: AgentToolRegistry,
    pub tool_ctx: InferenceContext,
    pub cancel_token: CancellationToken,
}

impl ChatSessionContext {
    pub fn agent_context_scope(&self) -> crate::agent::context::AgentContextScope<'_> {
        self.tool_ctx.agent_context_scope()
    }

    pub async fn build(
        harness: &Harness,
        user_id: &str,
        chat: Chat,
        cancel_token: CancellationToken,
        builder: Arc<dyn ConversationBuilder>,
        tool_filters: &[ToolFilter],
    ) -> Result<Self, AppError> {
        Self::build_for_response(
            harness,
            user_id,
            chat,
            None,
            cancel_token,
            builder,
            tool_filters,
        )
        .await
    }

    /// Agent switches reuse the already-initialized turn.
    pub(crate) async fn build_for_response(
        harness: &Harness,
        user_id: &str,
        chat: Chat,
        response_id: Option<&str>,
        cancel_token: CancellationToken,
        builder: Arc<dyn ConversationBuilder>,
        tool_filters: &[ToolFilter],
    ) -> Result<Self, AppError> {
        let event_sender: EventSender =
            harness
                .broadcast_service
                .create_event_sender(user_id, &chat.id, chat.space_id.clone());
        let agent_config = harness
            .chat_service
            .resolve_agent_config(&chat.agent_id)
            .await?;

        let agent = harness
            .agent_service
            .find_by_id(&chat.agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        let user = harness
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;

        // Load task early so `build_agent_registry` can register
        // task-domain tools in the same pass.
        let task = if let Some(ref task_id) = chat.task_id {
            Some(
                harness
                    .task_service
                    .find_by_id(task_id)
                    .await?
                    .ok_or_else(|| AppError::NotFound("Task".into()))?,
            )
        } else {
            None
        };
        let task_in_progress = task.as_ref().is_some_and(|t| {
            !matches!(t.kind, crate::agent::task::models::TaskKind::Cron { .. })
                && matches!(
                    t.status,
                    crate::agent::task::models::TaskStatus::Pending
                        | crate::agent::task::models::TaskStatus::InProgress
                )
        });
        let task_ctx = if task_in_progress {
            task.clone().map(|t| crate::tool::manager::TaskToolContext {
                task: t,
                storage_service: harness.storage_service.clone(),
                prompts: harness.prompts.clone(),
                chat_service: harness.chat_service.clone(),
                task_service: harness.task_service.clone(),
            })
        } else {
            None
        };

        let mut tool_registry = harness
            .tool_manager
            .build_agent_registry(user_id, &agent, &harness.policy_service, task_ctx)
            .await;

        // `send_message` initiates a new user-facing message; it only makes sense
        // when the agent is firing autonomously in its heartbeat chat. In a task
        // chat the delivery channel is `complete_task.result`; in a normal chat
        // the agent already replies by streaming. Allowing it elsewhere lets the
        // model duplicate work or, worse, satisfy a "send a reminder" instruction
        // via `send_message` and then leave `complete_task.result` empty against
        // a non-nullable schema.
        let in_heartbeat_chat = agent.heartbeat_chat_id.as_deref() == Some(&chat.id);
        if !in_heartbeat_chat {
            tool_registry.deny(&["send_message"]);
        }

        harness
            .mcp_service
            .register_agent_tool(&mut tool_registry, &harness.prompts);

        let model_group = harness
            .chat_service
            .model_providers()
            .resolve_with_fallback(
                &frona_harness::inference::ModelRef(agent_config.model_group.clone().into()),
                &frona_harness::inference::ModelRef::PRIMARY,
            )?;

        let loaded = harness
            .chat_service
            .compactor()
            .load_conversation(&chat.id)
            .await?;
        let agent_context = loaded.agent_context;
        let conversation_summary = loaded.summary;
        let mut stored_messages = loaded.messages;
        if let Some(response_id) = response_id {
            let response_index = stored_messages
                .iter()
                .position(|m| m.id == response_id)
                .ok_or_else(|| {
                    AppError::Conflict("Response is outside the active conversation".into())
                })?;
            stored_messages.truncate(response_index + 1);
        }
        let tool_calls = harness.chat_service.get_tool_calls(&chat.id).await?;

        // Cron is already filtered from `task_in_progress`: TASK.md would prompt
        // complete_task → status=Completed → cron stops firing forever.
        let task_prompt = task_in_progress
            .then(|| harness.prompts.read("TASK.md"))
            .flatten();

        if task_in_progress {
            tool_registry.apply_filter(&crate::tool::registry::ToolFilter::DenyList(&[
                "create_recurring_task",
            ]));
        }

        let model_config = model_group.main.clone();
        let conv_ctx = ConversationContext {
            agent_id: chat.agent_id.clone(),
            model_config,
            user_id: user_id.to_string(),
        };

        let mut model_history = builder
            .build(
                &stored_messages,
                &tool_calls,
                &conv_ctx,
                conversation_summary.as_deref(),
            )
            .await;
        model_history.set_summary_context(agent_context.as_deref());

        let mut file_paths = Vec::new();
        for msg in &stored_messages {
            for att in &msg.attachments {
                let resolved =
                    resolve_attachment_path(att, &harness.user_service, &harness.storage_service)
                        .await;
                if !file_paths.contains(&resolved) {
                    file_paths.push(resolved);
                }
            }
        }

        let extra = builder.system_prompt();
        let extra = extra
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let additional_instructions: Vec<&str> =
            task_prompt.as_deref().into_iter().chain(extra).collect();
        let system_prompt = harness
            .chat_service
            .build_system_prompt(&agent_config, &user, &agent, &additional_instructions)
            .await?;

        let mut tool_ctx = InferenceContext::new(
            user,
            agent,
            chat.clone(),
            event_sender,
            harness.shutdown_token.clone(),
            cancel_token.clone(),
        );
        tool_ctx.file_paths = file_paths;
        tool_ctx.task = task;

        let rig_history;

        for filter in tool_filters {
            tool_registry.apply_filter(filter);
        }
        let tools = crate::inference::tool_loop::to_rig_tool_definitions(
            tool_registry.definitions(),
            tool_registry.mcp_bridge_mode(),
        );
        let compacted = harness
            .chat_service
            .compactor()
            .compact_chat(
                &chat.id,
                super::compactor::CompactionContext {
                    agent_context: tool_ctx.agent_context_scope(),
                    system_prompt: &system_prompt,
                    model_group: &model_group,
                    model_history: &model_history,
                    tools: &tools,
                },
            )
            .await?;
        let input_token_estimate = compacted.input_token_estimate;
        model_history = compacted.model_history;
        rig_history = model_history.history.clone();
        super::compactor::validate_initial_context(
            &rig_history,
            &system_prompt,
            &model_group,
            &tools,
            input_token_estimate,
        )?;

        Ok(Self {
            chat,
            agent_config,
            system_prompt,
            model_group,
            rig_history,
            model_history,
            input_token_estimate,
            builder,
            tool_filters: tool_filters.to_vec(),

            tool_registry,
            tool_ctx,
            cancel_token,
        })
    }
}
