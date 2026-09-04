use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::prompt::PromptLoader;
use crate::agent::service::AgentService;
use crate::agent::skill::service::SkillService;
use crate::agent::task::service::TaskService;
use crate::auth::UserService;
use crate::chat::broadcast::BroadcastService;
use crate::chat::command::{CommandContext, CommandOutcome, CommandRegistry};
use crate::chat::message::models::{Message, MessageCommand, MessageRole};
use crate::chat::service::ChatService;
use crate::chat::session::ChatSessionContext;
use crate::core::config::Config;
use crate::core::error::AppError;
use crate::core::execution::{
    ExecutionKind, ExecutionRegistry, ExecutionSource, ExecutionSourceKind, NewExecution,
};
use crate::core::state::ActiveSessions;
use crate::credential::vault::service::VaultService;
use crate::inference::config::ModelGroup;
use crate::inference::conversation::{ConversationBuilder, DefaultConversationBuilder};
use crate::inference::hitl::{HitlOutcome, HitlResponse, ResolveOutcome};
use crate::inference::request::{InferenceContext, InferenceRequest, InferenceResponse};
use crate::inference::tool_call::ToolStatus;
use crate::inference::usage::UsageContext;
use crate::memory::service::MemoryService;
use crate::policy::service::PolicyService;
use crate::storage::StorageService;
use crate::tool::AgentTool;
use crate::tool::manager::ToolManager;
use crate::tool::mcp::McpServerService;
use crate::tool::registry::ToolFilter;
use rig_core::completion::Message as RigMessage;

pub struct AgentLoopOutcome {
    /// What inference produced (Completed text, Cancelled, ExternalToolPending,
    /// or Handled when a command short-circuited the turn).
    pub inference: InferenceResponse,
    /// The in-flight agent message that the reply gets written into. Already
    /// reflects any mutations a command handler made (e.g. `agent_id` swap
    /// from `SwitchAgentCommand`). Callers pass this into the terminal-write
    /// APIs (`complete_agent_message`, `cancel_agent_message`, etc.) instead
    /// of fetching by id, so the handler's mutations land in a single write.
    pub response: Message,
}

/// Field typing mirrors AppState: bare types for services that derive `Clone`
/// internally (their fields are already `Arc`-wrapped), explicit `Arc<T>` for
/// services holding non-Clone state (`OnceLock`, `RwLock`, large config).
pub struct Harness {
    pub(crate) chat_service: ChatService,
    pub(crate) user_service: UserService,
    pub(crate) storage_service: StorageService,
    pub(crate) agent_service: AgentService,
    pub(crate) memory_service: Arc<dyn MemoryService>,
    pub(crate) skill_service: SkillService,
    pub(crate) task_service: TaskService,
    pub(crate) vault_service: VaultService,
    pub(crate) mcp_service: Arc<McpServerService>,
    pub(crate) tool_manager: Arc<ToolManager>,
    pub(crate) policy_service: PolicyService,
    pub(crate) broadcast_service: BroadcastService,
    pub(crate) active_sessions: ActiveSessions,
    pub(crate) execution_registry: ExecutionRegistry,
    pub(crate) shutdown_token: CancellationToken,
    pub(crate) prompts: PromptLoader,
    pub(crate) config: Arc<Config>,
    pub(crate) commands: Arc<CommandRegistry>,
    pub(crate) usage_service: crate::inference::usage::UsageService,
}

impl Harness {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chat_service: ChatService,
        user_service: UserService,
        storage_service: StorageService,
        agent_service: AgentService,
        memory_service: Arc<dyn MemoryService>,
        skill_service: SkillService,
        task_service: TaskService,
        vault_service: VaultService,
        mcp_service: Arc<McpServerService>,
        tool_manager: Arc<ToolManager>,
        policy_service: PolicyService,
        broadcast_service: BroadcastService,
        active_sessions: ActiveSessions,
        execution_registry: ExecutionRegistry,
        shutdown_token: CancellationToken,
        prompts: PromptLoader,
        config: Arc<Config>,
        usage_service: crate::inference::usage::UsageService,
    ) -> Self {
        let mut registry = CommandRegistry::new();
        crate::chat::command::builtin::register_all(&mut registry);
        let commands = Arc::new(registry);

        Self {
            chat_service,
            user_service,
            storage_service,
            agent_service,
            memory_service,
            skill_service,
            task_service,
            vault_service,
            mcp_service,
            tool_manager,
            policy_service,
            broadcast_service,
            active_sessions,
            execution_registry,
            shutdown_token,
            prompts,
            config,
            commands,
            usage_service,
        }
    }

    pub async fn structured_inference<T>(
        &self,
        model_group: &ModelGroup,
        system: &str,
        history: Vec<RigMessage>,
        usage_ctx: UsageContext,
    ) -> Result<T, AppError>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        crate::inference::structured_inference::<T>(
            self.chat_service.provider_registry(),
            model_group,
            system,
            history,
            &self.usage_service,
            &usage_ctx,
        )
        .await
        .map_err(|e| AppError::Internal(format!("harness structured inference: {e}")))
    }

    pub async fn text_inference(
        &self,
        model_group: &ModelGroup,
        system: &str,
        history: Vec<RigMessage>,
        usage_ctx: UsageContext,
    ) -> Result<String, AppError> {
        crate::inference::text_inference(
            self.chat_service.provider_registry(),
            model_group,
            system,
            history,
            &self.usage_service,
            &usage_ctx,
        )
        .await
        .map_err(|e| AppError::Internal(format!("harness text inference: {e}")))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn text_inference_with_tools(
        &self,
        agent_id: &str,
        model_group: &ModelGroup,
        system: &str,
        history: Vec<RigMessage>,
        tool_filters: &[ToolFilter],
        extra_tools: &[Arc<dyn AgentTool>],
        max_turns: usize,
        usage_ctx: UsageContext,
    ) -> Result<String, AppError> {
        self.text_inference_with_tools_cancel(
            agent_id,
            model_group,
            system,
            history,
            tool_filters,
            extra_tools,
            max_turns,
            usage_ctx,
            CancellationToken::new(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn text_inference_with_tools_cancel(
        &self,
        agent_id: &str,
        model_group: &ModelGroup,
        system: &str,
        history: Vec<RigMessage>,
        tool_filters: &[ToolFilter],
        extra_tools: &[Arc<dyn AgentTool>],
        max_turns: usize,
        usage_ctx: UsageContext,
        cancel_token: CancellationToken,
    ) -> Result<String, AppError> {
        let user_id = &usage_ctx.user_id;
        let agent = self
            .agent_service
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent not found: {agent_id}")))?;
        let mut tools = self
            .tool_manager
            .build_agent_registry(user_id, &agent, &self.policy_service, None)
            .await;
        for filter in tool_filters {
            tools.apply_filter(filter);
        }
        for tool in extra_tools {
            tools.register_required(tool.clone())?;
        }
        if tools.is_empty() {
            return tokio::select! {
                _ = cancel_token.cancelled() => Err(AppError::Internal("background inference cancelled".into())),
                result = self.text_inference(model_group, system, history, usage_ctx) => result,
            };
        }
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("User not found: {user_id}")))?;
        let ctx =
            InferenceContext::new_detached(user, agent, self.shutdown_token.clone(), cancel_token);
        crate::inference::structured::text_inference_with_tools(
            self.chat_service.provider_registry(),
            model_group,
            system,
            history,
            &tools,
            &ctx,
            &self.usage_service,
            &usage_ctx,
            max_turns,
        )
        .await
    }

    /// Agentic structured inference: builds a background `InferenceContext` + a
    /// policy-filtered, caller-restricted tool registry (via `tool_filters`), then
    /// runs the tool loop. Falls back to a plain (tool-less) structured call if the
    /// agent/context can't be built or no tools survive the filters.
    /// `extra_tools` are caller-owned tool instances registered **after** the Cedar-gated
    /// agent registry and its filters - so they bypass both Cedar and `tool_filters`. Use
    /// for tools scoped to *this* task that must never surface on a normal agent turn (e.g.
    /// consolidation's `get_invocation_output`). Pass `&[]` for the ordinary case.
    #[allow(clippy::too_many_arguments)]
    pub async fn structured_inference_with_tools<T>(
        &self,
        chat_id: Option<&str>,
        agent_id: &str,
        model_group: &ModelGroup,
        system: &str,
        history: Vec<RigMessage>,
        tool_filters: &[ToolFilter],
        extra_tools: &[Arc<dyn AgentTool>],
        max_turns: usize,
        usage_ctx: UsageContext,
    ) -> Result<T, AppError>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        let user_id = &usage_ctx.user_id;
        let registry = self.chat_service.provider_registry();

        let agent = self
            .agent_service
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent not found: {agent_id}")))?;

        let mut tools = self
            .tool_manager
            .build_agent_registry(user_id, &agent, &self.policy_service, None)
            .await;
        for f in tool_filters {
            tools.apply_filter(f);
        }
        for t in extra_tools {
            tools.register_required(t.clone())?;
        }

        if tools.is_empty() {
            return crate::inference::structured_inference::<T>(
                registry,
                model_group,
                system,
                history,
                &self.usage_service,
                &usage_ctx,
            )
            .await
            .map_err(|e| AppError::Internal(format!("harness structured inference: {e}")));
        }

        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("User not found: {user_id}")))?;

        // `Some(chat_id)` → bind to that chat (streams events); `None` → detached
        // (no chat, no event streaming - chat-scoped tools refuse via `active_chat`).
        let ctx = match chat_id {
            Some(chat_id) => {
                let chat = self
                    .chat_service
                    .find_chat(chat_id)
                    .await?
                    .ok_or_else(|| AppError::NotFound(format!("Chat not found: {chat_id}")))?;
                let event_tx = self.broadcast_service.create_event_sender(
                    user_id,
                    chat_id,
                    chat.space_id.clone(),
                );
                InferenceContext::new(
                    user,
                    agent,
                    chat,
                    event_tx,
                    self.shutdown_token.clone(),
                    CancellationToken::new(),
                )
            }
            None => InferenceContext::new_detached(
                user,
                agent,
                self.shutdown_token.clone(),
                CancellationToken::new(),
            ),
        };
        crate::inference::structured_inference_with_tools::<T>(
            registry,
            model_group,
            system,
            history,
            &tools,
            &ctx,
            &self.usage_service,
            &usage_ctx,
            max_turns,
        )
        .await
    }

    /// Begin a NON-PERSISTENT, structured tool dialogue that yields a `T` (see
    /// [`crate::inference::StructuredConversation`]). Resolves the agent, builds its
    /// tool registry (+ `extra_tools`) and a context (detached if `chat_id` is `None`),
    /// seeds the conversation with `system` + `initial`, and returns a handle the caller
    /// drives with `next_attempt()` / `reject_submission()`. Writes no chat/message rows.
    /// `max_tool_turns` bounds exploration; the caller separately bounds answer attempts.
    #[allow(clippy::too_many_arguments)]
    pub async fn structured_conversation<'a, T>(
        &'a self,
        chat_id: Option<&str>,
        agent_id: &str,
        model_group: &ModelGroup,
        system: impl Into<String>,
        initial: impl Into<String>,
        tool_filters: &[ToolFilter],
        extra_tools: &[Arc<dyn AgentTool>],
        max_tool_turns: usize,
        usage_ctx: UsageContext,
    ) -> Result<crate::inference::StructuredConversation<'a, T>, AppError>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        self.structured_conversation_with_cancel(
            chat_id,
            agent_id,
            model_group,
            system,
            initial,
            tool_filters,
            extra_tools,
            max_tool_turns,
            usage_ctx,
            CancellationToken::new(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn structured_conversation_with_cancel<'a, T>(
        &'a self,
        chat_id: Option<&str>,
        agent_id: &str,
        model_group: &ModelGroup,
        system: impl Into<String>,
        initial: impl Into<String>,
        tool_filters: &[ToolFilter],
        extra_tools: &[Arc<dyn AgentTool>],
        max_tool_turns: usize,
        usage_ctx: UsageContext,
        cancel_token: CancellationToken,
    ) -> Result<crate::inference::StructuredConversation<'a, T>, AppError>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        let user_id = &usage_ctx.user_id;
        let registry = self.chat_service.provider_registry();

        let agent = self
            .agent_service
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent not found: {agent_id}")))?;

        let mut tools = self
            .tool_manager
            .build_agent_registry(user_id, &agent, &self.policy_service, None)
            .await;
        for f in tool_filters {
            tools.apply_filter(f);
        }
        for t in extra_tools {
            tools.register_required(t.clone())?;
        }

        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("User not found: {user_id}")))?;

        let ctx = match chat_id {
            Some(chat_id) => {
                let chat = self
                    .chat_service
                    .find_chat(chat_id)
                    .await?
                    .ok_or_else(|| AppError::NotFound(format!("Chat not found: {chat_id}")))?;
                let event_tx = self.broadcast_service.create_event_sender(
                    user_id,
                    chat_id,
                    chat.space_id.clone(),
                );
                InferenceContext::new(
                    user,
                    agent,
                    chat,
                    event_tx,
                    self.shutdown_token.clone(),
                    cancel_token.clone(),
                )
            }
            None => InferenceContext::new_detached(
                user,
                agent,
                self.shutdown_token.clone(),
                cancel_token,
            ),
        };
        Ok(crate::inference::StructuredConversation::new(
            registry,
            &self.usage_service,
            tools,
            ctx,
            model_group.clone(),
            system.into(),
            initial.into(),
            usage_ctx,
            max_tool_turns,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        user_id: &str,
        chat_id: &str,
        message_id: &str,
        cancel_token: CancellationToken,
        builder: Box<dyn ConversationBuilder>,
        tool_filters: &[ToolFilter],
        command_context_registry: Option<Arc<CommandRegistry>>,
    ) {
        let chat = self.chat_service.find_chat(chat_id).await.ok().flatten();
        let agent_name = match chat.as_ref() {
            Some(chat) => self
                .agent_service
                .find_by_id(&chat.agent_id)
                .await
                .ok()
                .flatten()
                .map(|agent| agent.name),
            None => None,
        };
        let title = chat
            .and_then(|chat| chat.title)
            .unwrap_or_else(|| "Assistant response".to_string());
        let execution = NewExecution {
            title,
            agent_name,
            kind: ExecutionKind::Inference,
            action: Some("Generating response".to_string()),
            source: Some(ExecutionSource {
                kind: ExecutionSourceKind::Chat,
                id: Some(chat_id.to_string()),
            }),
            related_chat_ids: vec![chat_id.to_string()],
            can_cancel: true,
        };
        self.run_turn_with_execution(
            user_id,
            chat_id,
            message_id,
            cancel_token,
            builder,
            tool_filters,
            command_context_registry,
            execution,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn_with_execution(
        &self,
        user_id: &str,
        chat_id: &str,
        message_id: &str,
        cancel_token: CancellationToken,
        builder: Box<dyn ConversationBuilder>,
        tool_filters: &[ToolFilter],
        command_context_registry: Option<Arc<CommandRegistry>>,
        execution: NewExecution,
    ) {
        let _execution = self.execution_registry.start(user_id, execution);
        let outcome = self
            .run_loop(
                user_id,
                chat_id,
                message_id,
                cancel_token,
                builder,
                tool_filters,
                command_context_registry,
            )
            .await;
        self.finalize(message_id, user_id, outcome).await;
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_loop(
        &self,
        user_id: &str,
        chat_id: &str,
        message_id: &str,
        cancel_token: CancellationToken,
        builder: Box<dyn ConversationBuilder>,
        tool_filters: &[ToolFilter],
        command_context_registry: Option<Arc<CommandRegistry>>,
    ) -> Result<AgentLoopOutcome, AppError> {
        let mut chat = self
            .chat_service
            .find_chat(chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;

        // `message_id` is the AGENT response placeholder. The user message is
        // separate. Signal/system-only chats may have no user-role message at
        // all - then there's nothing to dispatch and we go straight to inference.
        let request = self
            .chat_service
            .get_stored_messages(chat_id)
            .await?
            .into_iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::User));

        let mut response = self.chat_service.get_message(user_id, message_id).await?;

        let builder_system_prompt = builder.system_prompt();

        let mut session =
            ChatSessionContext::build(self, user_id, chat.clone(), cancel_token.clone(), builder)
                .await?;

        let mut prompt_override: Option<String> = None;
        if let Some(mut request) = request
            && matches!(request.role, MessageRole::User)
            && let Some(MessageCommand::Command { name, args }) = request.command.clone()
        {
            let user = self
                .user_service
                .find_by_id(user_id)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("user {user_id}")))?;

            let cmd = match command_context_registry.as_ref().and_then(|r| r.get(&name)) {
                Some(c) => Some(c),
                None => self.commands.resolve(&name, self, &user).await,
            };
            let cmd = cmd.ok_or_else(|| {
                AppError::NotFound(format!("command '{name}' not registered for this chat"))
            })?;

            // `response` is always written via the terminal API at end-of-turn,
            // so no snapshot is needed for it - only chat/request.
            let chat_snapshot = chat.clone();
            let request_snapshot = request.clone();

            let mut cmd_ctx = CommandContext {
                harness: self,
                session: &mut session,
                user: &user,
                chat: &mut chat,
                request: &mut request,
                response: &mut response,
            };

            let outcome = cmd.run(&args, &mut cmd_ctx).await;

            if chat != chat_snapshot {
                let _ = self.chat_service.save_chat(&chat).await;
            }
            if request != request_snapshot {
                let _ = self.chat_service.save_updated_message(&request).await;
            }

            match outcome {
                Ok(CommandOutcome::Prompt(rendered)) => {
                    prompt_override = Some(rendered);
                }
                Ok(CommandOutcome::Message(text)) => {
                    response.content = text;
                    let _ = self
                        .chat_service
                        .complete_agent_message(response.clone())
                        .await;
                    return Ok(AgentLoopOutcome {
                        inference: InferenceResponse::Handled,
                        response,
                    });
                }
                Ok(CommandOutcome::End) => {
                    let _ = self
                        .chat_service
                        .cancel_agent_message(response.clone())
                        .await;
                    return Ok(AgentLoopOutcome {
                        inference: InferenceResponse::Handled,
                        response,
                    });
                }
                Err(e) => {
                    response.content = format!("Command failed: {e}");
                    let _ = self
                        .chat_service
                        .complete_agent_message(response.clone())
                        .await;
                    return Ok(AgentLoopOutcome {
                        inference: InferenceResponse::Handled,
                        response,
                    });
                }
            }
        }

        let ChatSessionContext {
            mut system_prompt,
            model_group,
            mut rig_history,
            registry,
            mut tool_registry,
            tool_ctx,
            ..
        } = session;

        if let Some(extra) = builder_system_prompt {
            let trimmed = extra.trim();
            if !trimmed.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(trimmed);
            }
        }

        for filter in tool_filters {
            tool_registry.apply_filter(filter);
        }

        // Swap only the model's view; the persisted `Message.content` is untouched.
        if let Some(rendered) = prompt_override
            && let Some(last_user) = rig_history
                .iter_mut()
                .rev()
                .find(|m| matches!(m, rig_core::completion::Message::User { .. }))
        {
            *last_user = rig_core::completion::Message::user(rendered);
        }

        let inference = crate::inference::inference(InferenceRequest {
            registry,
            model_group,
            system_prompt,
            history: rig_history,
            tool_registry,
            ctx: tool_ctx,
            cancel_token,
            chat_service: self.chat_service.clone(),
            message_id: message_id.to_string(),
            usage_service: self.usage_service.clone(),
        })
        .await?;

        Ok(AgentLoopOutcome {
            inference,
            response,
        })
    }

    pub async fn resume(
        &self,
        user_id: &str,
        chat_id: &str,
        message_id: &str,
    ) -> Result<(), AppError> {
        let cancel_token = self.active_sessions.register(chat_id).await;
        let builder = Box::new(DefaultConversationBuilder {
            user_service: self.user_service.clone(),
            storage_service: self.storage_service.clone(),
            agent_service: self.agent_service.clone(),
        });
        self.run_turn(
            user_id,
            chat_id,
            message_id,
            cancel_token,
            builder,
            &[],
            None,
        )
        .await;
        self.active_sessions.remove(chat_id).await;
        Ok(())
    }

    pub async fn resume_with_execution(
        &self,
        user_id: &str,
        chat_id: &str,
        message_id: &str,
        execution: NewExecution,
    ) -> Result<(), AppError> {
        let cancel_token = self.active_sessions.register(chat_id).await;
        let builder = Box::new(DefaultConversationBuilder {
            user_service: self.user_service.clone(),
            storage_service: self.storage_service.clone(),
            agent_service: self.agent_service.clone(),
        });
        self.run_turn_with_execution(
            user_id,
            chat_id,
            message_id,
            cancel_token,
            builder,
            &[],
            None,
            execution,
        )
        .await;
        self.active_sessions.remove(chat_id).await;
        Ok(())
    }

    /// Does NOT spawn a resume - the caller dispatches via
    /// `state.task_executor.resume_or_notify(...)` when `should_resume`.
    pub async fn resolve_and_resume(
        &self,
        tool_call_id: &str,
        response: HitlResponse,
    ) -> Result<ResolveOutcome, AppError> {
        let te = self
            .chat_service
            .get_tool_call(tool_call_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("tool_call {tool_call_id}")))?;

        let hitl = te
            .hitl
            .as_ref()
            .ok_or_else(|| AppError::Validation(format!("tool_call {tool_call_id} has no HITL")))?;

        if matches!(hitl.status, ToolStatus::Resolved | ToolStatus::Denied) {
            return Ok(ResolveOutcome::AlreadyResolved);
        }

        let tool = self
            .tool_manager
            .find_tool_for_resume(&te.name)
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "no tool registered to handle resume for '{}'",
                    te.name
                ))
            })?;

        let chat = self
            .chat_service
            .find_chat(&te.chat_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("chat {}", te.chat_id)))?;
        let user = self
            .user_service
            .find_by_id(&chat.user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("user {}", chat.user_id)))?;
        let agent = self
            .agent_service
            .find_by_id(&chat.agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("agent {}", chat.agent_id)))?;
        let event_tx = self.broadcast_service.create_event_sender(
            &user.id,
            &te.chat_id,
            chat.space_id.clone(),
        );
        let ctx = InferenceContext::new(
            user.clone(),
            agent,
            chat.clone(),
            event_tx,
            self.shutdown_token.clone(),
            CancellationToken::new(),
        );

        let request = hitl.request.clone();
        let outcome = tool
            .on_resume(&te.name, &request, response.clone(), &ctx)
            .await?;

        let resolved_message = match outcome {
            HitlOutcome::Resolved(text) => {
                self.chat_service
                    .resolve_tool_call_with_hitl_response(tool_call_id, Some(text), Some(response))
                    .await?
            }
            HitlOutcome::Denied(text) => {
                self.chat_service
                    .deny_tool_call_with_hitl_response(tool_call_id, Some(text), Some(response))
                    .await?
            }
        };

        let message_response = match resolved_message {
            crate::chat::service::ToolResolveResult::Changed(m)
            | crate::chat::service::ToolResolveResult::AlreadyResolved(m) => m,
        };

        self.broadcast_service
            .send(crate::chat::broadcast::BroadcastEvent {
                user_id: user.id.clone(),
                chat_id: Some(te.chat_id.clone()),
                space_id: chat.space_id.clone(),
                kind: crate::chat::broadcast::BroadcastEventKind::Inference(
                    crate::inference::tool_loop::InferenceEventKind::Resume {
                        message: message_response,
                    },
                ),
            });

        let did_flip = self
            .chat_service
            .mark_message_executing(&te.message_id)
            .await
            .unwrap_or(false);

        Ok(ResolveOutcome::Resolved {
            should_resume: did_flip,
            user_id: user.id.clone(),
            chat_id: te.chat_id.clone(),
            message_id: te.message_id.clone(),
            task_id: chat.task_id.clone(),
        })
    }

    pub async fn resume_all(self: &Arc<Self>) {
        let executing: Vec<Message> = self.chat_service.find_executing_chat_messages().await;
        if executing.is_empty() {
            return;
        }
        tracing::info!(
            count = executing.len(),
            "Resuming interrupted chats from previous run"
        );
        for msg in executing {
            let this = Arc::clone(self);
            let chat_id = msg.chat_id.clone();
            let msg_id = msg.id.clone();
            tokio::spawn(async move {
                let user_id = match this.chat_service.find_chat(&chat_id).await {
                    Ok(Some(chat)) => chat.user_id,
                    _ => {
                        tracing::error!(chat_id = %chat_id, "Failed to find chat for resume");
                        return;
                    }
                };
                if let Err(e) = this.resume(&user_id, &chat_id, &msg_id).await {
                    tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume chat");
                }
            });
        }
    }

    async fn finalize(
        &self,
        message_id: &str,
        user_id: &str,
        outcome: Result<AgentLoopOutcome, AppError>,
    ) {
        match outcome {
            Ok(AgentLoopOutcome {
                inference,
                mut response,
            }) => match inference {
                InferenceResponse::Completed {
                    text,
                    attachments,
                    reasoning,
                    ..
                } => {
                    response.content = text;
                    response.attachments = attachments;
                    response.reasoning = reasoning;
                    let _ = self.chat_service.complete_agent_message(response).await;
                }
                InferenceResponse::Cancelled(text) => {
                    response.content = text;
                    let _ = self.chat_service.cancel_agent_message(response).await;
                }
                InferenceResponse::ExternalToolPending { tool_calls, .. } => {
                    let _ = self
                        .chat_service
                        .pause_agent_message(
                            response,
                            crate::inference::tool_loop::PauseReason::Hitl,
                            tool_calls,
                        )
                        .await;
                }
                InferenceResponse::Handled => {
                    // Command dispatch already wrote/cancelled the response.
                }
            },
            Err(e) => {
                tracing::warn!(message_id, error = %e, "agent loop failed");
                if let Ok(msg) = self.chat_service.get_message(user_id, message_id).await {
                    let _ = self
                        .chat_service
                        .fail_agent_message(msg, e.to_string())
                        .await;
                }
            }
        }
    }
}
