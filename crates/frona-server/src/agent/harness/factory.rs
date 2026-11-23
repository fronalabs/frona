//! Bind an SDK harness to an agent and, optionally, a chat.
use async_trait::async_trait;
use frona_harness::{
    ExecutionStop, HistoryEvent, HistoryRecord, HistoryWriteError, HistoryWritePhase,
    HistoryWriter, Tool, ToolContext, ToolResult,
};
use futures::{FutureExt, future::BoxFuture};
use rig_core::completion::request::ToolDefinition;
use serde_json::Value;
use std::sync::Arc;

use super::Harness;
use crate::{
    chat::message::error::MessageError,
    core::error::AppError,
    inference::usage::{InferenceKind, UsageContext},
    tool::{InferenceContext, registry::AgentToolRegistry},
};

/// Chat selected by a factory method and opened when the run starts.
enum ChatDestination<'a> {
    Existing(&'a str),
    New { title: &'a str },
}

impl ChatDestination<'_> {
    /// Return the destination ID and the operation that opens it at execution time.
    fn bind<'a>(
        self,
        service: &'a crate::chat::service::ChatService,
        user_id: &str,
        agent_id: &str,
    ) -> (
        String,
        BoxFuture<'a, Result<crate::chat::models::Chat, AppError>>,
    ) {
        use crate::chat::models::CreateChatRequest;
        match self {
            Self::Existing(id) => {
                let id = id.to_owned();
                let owner = user_id.to_owned();
                let agent = agent_id.to_owned();
                let destination = id.clone();
                (
                    id,
                    async move { owned_agent_chat(service, &owner, &agent, &destination).await }
                        .boxed(),
                )
            }
            Self::New { title } => {
                let id = crate::core::repository::new_id();
                let request = CreateChatRequest {
                    agent_id: agent_id.to_owned(),
                    title: Some(title.to_owned()),
                    space_id: None,
                    task_id: None,
                    metadata: None,
                };
                let owner = user_id.to_owned();
                let new_id = id.clone();
                (
                    id,
                    async move { service.create_chat_with_id(&owner, new_id, request).await }
                        .boxed(),
                )
            }
        }
    }
}

async fn owned_agent_chat(
    service: &crate::chat::service::ChatService,
    user_id: &str,
    agent_id: &str,
    chat_id: &str,
) -> Result<crate::chat::models::Chat, AppError> {
    let chat = service.get_chat(user_id, chat_id).await?;
    if chat.agent_id != agent_id {
        return Err(AppError::Forbidden(
            "Chat belongs to a different agent".into(),
        ));
    }
    Ok(chat)
}

/// Host-side settings for an SDK run shown in chat activity.
pub struct ChatRunOptions<'a> {
    pub tools: &'a [&'a str],
    pub kind: crate::core::execution::ExecutionKind,
    pub action: &'a str,
    pub related_chat_ids: &'a [&'a str],
}

pub type AgentHarness<'a> =
    frona_harness::Harness<'static, frona_harness::RunBinding<'a, AppError>>;

impl Default for ChatRunOptions<'_> {
    fn default() -> Self {
        Self {
            tools: &[],
            kind: crate::core::execution::ExecutionKind::Inference,
            action: "Generating response",
            related_chat_ids: &[],
        }
    }
}

impl Harness {
    /// Prepare a run without a chat.
    pub async fn prepare(
        &self,
        user_id: &str,
        agent_id: &str,
    ) -> Result<AgentHarness<'_>, AppError> {
        self.build(user_id, agent_id, None, ChatRunOptions::default())
            .await
    }

    /// Use an existing chat and its owning agent.
    pub async fn prepare_for_chat(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<AgentHarness<'_>, AppError> {
        self.prepare_for_chat_with_options(user_id, chat_id, ChatRunOptions::default())
            .await
    }

    pub async fn prepare_for_chat_with_options(
        &self,
        user_id: &str,
        chat_id: &str,
        options: ChatRunOptions<'_>,
    ) -> Result<AgentHarness<'_>, AppError> {
        let chat = self.chat_service.get_chat(user_id, chat_id).await?;
        self.build(
            user_id,
            &chat.agent_id,
            Some(ChatDestination::Existing(chat_id)),
            options,
        )
        .await
    }

    /// Reserve a chat ID and create the chat when execution starts.
    pub async fn prepare_with_new_chat(
        &self,
        user_id: &str,
        agent_id: &str,
        title: &str,
    ) -> Result<AgentHarness<'_>, AppError> {
        self.prepare_with_new_chat_options(user_id, agent_id, title, ChatRunOptions::default())
            .await
    }

    pub async fn prepare_with_new_chat_options(
        &self,
        user_id: &str,
        agent_id: &str,
        title: &str,
        options: ChatRunOptions<'_>,
    ) -> Result<AgentHarness<'_>, AppError> {
        self.build(
            user_id,
            agent_id,
            Some(ChatDestination::New { title }),
            options,
        )
        .await
    }

    async fn build(
        &self,
        user_id: &str,
        agent_id: &str,
        destination: Option<ChatDestination<'_>>,
        options: ChatRunOptions<'_>,
    ) -> Result<AgentHarness<'_>, AppError> {
        use crate::core::execution::{ExecutionSource, ExecutionSourceKind, NewExecution};
        let agent = self.agent_service.get(user_id, agent_id).await?;
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;
        let config = self.chat_service.resolve_agent_config(agent_id).await?;
        let model = self.chat_service.model_providers().resolve_with_fallback(
            &frona_harness::inference::ModelRef(config.model_group.clone().into()),
            &frona_harness::inference::ModelRef::PRIMARY,
        )?;
        let model_name = model.name.clone();
        let instructions = self
            .chat_service
            .build_system_prompt(&config, &user, &agent, &[])
            .await?;
        let sdk = frona_harness::Harness::new(frona_harness::Model::from_group(model));
        let cancellation = self.shutdown_token.child_token();
        let Some(destination) = destination else {
            return Ok(sdk.bind(user.id, None, move |run, _| async move {
                Ok(run.instructions(instructions).cancellation(cancellation))
            }));
        };
        let (chat_id, chat) = destination.bind(&self.chat_service, user_id, agent_id);
        let mut registry = self
            .tool_manager
            .build_agent_registry(user_id, &agent, &self.policy_service, None)
            .await;
        for name in options.tools {
            if !registry.definitions().iter().any(|d| d.id == *name) {
                return Err(AppError::Forbidden(format!(
                    "Tool '{name}' is not available to this agent"
                )));
            }
        }
        registry.restrict_to(options.tools);
        let activity = NewExecution {
            title: options.action.to_owned(),
            agent_name: Some(agent.name.clone()),
            kind: options.kind,
            action: Some(options.action.to_owned()),
            source: Some(ExecutionSource {
                kind: ExecutionSourceKind::Chat,
                id: Some(chat_id.clone()),
            }),
            related_chat_ids: std::iter::once(chat_id.as_str())
                .chain(
                    options
                        .related_chat_ids
                        .iter()
                        .copied()
                        .filter(|id| *id != chat_id),
                )
                .map(str::to_owned)
                .collect(),
            can_cancel: true,
        };
        Ok(sdk.bind(
            user.id.clone(),
            Some(chat_id.clone()),
            move |run, input| async move {
                let chat = chat.await?;
                let mut activity = activity;
                if let Some(title) = &chat.title {
                    activity.title = title.clone();
                }
                let events = self.broadcast_service.create_event_sender(
                    &user.id,
                    &chat.id,
                    chat.space_id.clone(),
                );
                self.chat_service
                    .save_system_message(&user.id, chat.space_id.as_deref(), &chat.id, input)
                    .await?;
                let message = self
                    .chat_service
                    .create_application_agent_message(&chat.id, &agent.id)
                    .await?;
                let user_id = user.id.clone();
                let usage = UsageContext::new(
                    InferenceKind::ToolTurn {
                        agent_id: agent.id.clone(),
                        chat_id: chat_id.clone(),
                        message_id: message.id.clone(),
                        turn_index: 0,
                    },
                    &user_id,
                    model_name,
                );
                let ctx = Arc::new(InferenceContext::new(
                    user,
                    agent,
                    chat,
                    events,
                    self.shutdown_token.clone(),
                    cancellation.clone(),
                ));
                let history = crate::chat::service::history::MessageHistoryBinding::new(
                    &self.chat_service,
                    &ctx,
                    &message.id,
                )
                .await?;
                let registry = Arc::new(registry);
                let mut tools = frona_harness::ToolRegistry::default();
                for definition in crate::inference::tool_loop::to_rig_tool_definitions(
                    registry.definitions(),
                    false,
                ) {
                    tools
                        .register(ChatRunTool {
                            definition,
                            registry: registry.clone(),
                            ctx: ctx.clone(),
                            history: history.clone(),
                        })
                        .map_err(|e| AppError::Validation(e.to_string()))?;
                }
                let sessions = self.active_sessions.clone();
                let executions = self.execution_registry.clone();
                Ok(run
                    .instructions(instructions)
                    .tools(tools)
                    .observer(ChatUsageObserver {
                        service: self.usage_service.clone(),
                        context: usage,
                    })
                    .cancellation(cancellation)
                    .history_writer(FinalizingMessageHistoryWriter(history.into_writer()))
                    .tool_call_hook(RequireInlineContinuation)
                    .on_start(move |token| {
                        (
                            sessions.register_scoped(&chat_id, token),
                            executions.start(&user_id, activity.clone()),
                        )
                    }))
            },
        ))
    }
}

struct ChatUsageObserver {
    service: crate::inference::usage::UsageService,
    context: UsageContext,
}
#[async_trait]
impl frona_harness::inference::observation::InferenceObserver for ChatUsageObserver {
    async fn usage(
        &self,
        observation: frona_harness::inference::observation::UsageObservation<'_>,
    ) -> Result<(), frona_harness::inference::observation::ObservationError> {
        self.service.scoped(&self.context).usage(observation).await
    }
}

/// The shared writer owns storage. This wrapper supplies the message lifecycle
/// for runs whose caller does not use the ordinary chat finalizer.
struct FinalizingMessageHistoryWriter(crate::chat::service::history::MessageHistoryWriter);
impl std::fmt::Debug for FinalizingMessageHistoryWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
#[async_trait]
impl HistoryWriter for FinalizingMessageHistoryWriter {
    async fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        if let Err(error) = self.0.commit(record).await {
            if let Ok(message) = self.0.host.message().await
                && message.status == Some(crate::chat::message::models::MessageStatus::Executing)
            {
                let _ = self
                    .0
                    .host
                    .service
                    .fail_agent_message(
                        message,
                        MessageError::from(&AppError::Internal(error.cause.clone())),
                    )
                    .await;
            }
            return Err(error);
        }
        let result = async {
            match &record.event {
                HistoryEvent::SubmissionCompleted { .. } | HistoryEvent::Finished { .. } => {
                    let mut message = self.0.host.message().await?;
                    message.content = self.0.text.clone();
                    self.0.host.service.complete_agent_message(message).await?;
                }
                HistoryEvent::Stopped {
                    category,
                    message: reason,
                } => {
                    let mut message = self.0.host.message().await?;
                    message.content = self.0.text.clone();
                    if *category == frona_harness::FailureKind::Cancelled {
                        self.0.host.service.cancel_agent_message(message).await?;
                    } else {
                        self.0
                            .host
                            .service
                            .fail_agent_message(
                                message,
                                MessageError::from(&AppError::Tool(reason.clone())),
                            )
                            .await?;
                    }
                }
                _ => {}
            }
            Ok::<_, AppError>(())
        }
        .await;
        result.map_err(|e| HistoryWriteError {
            phase: HistoryWritePhase::Append,
            cause: e.to_string(),
        })
    }
}
struct ChatRunTool {
    definition: ToolDefinition,
    registry: Arc<AgentToolRegistry>,
    ctx: Arc<InferenceContext>,
    history: crate::chat::service::history::MessageHistoryBinding,
}
#[async_trait]
impl Tool for ChatRunTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn call(&mut self, arguments: Value, control: &ToolContext<'_>) -> ToolResult {
        let result = crate::tool::runtime_dispatch::invoke_runtime_tool(
            &self.history,
            &self.registry,
            &self.ctx,
            &self.definition.name,
            arguments,
            control,
        )
        .await;
        let with_prompt = |mut output: frona_harness::ToolOutput| {
            if let Some(prompt) = output
                .metadata
                .pointer("/frona_effects/system_prompt")
                .and_then(Value::as_str)
            {
                output
                    .content
                    .push(frona_harness::ToolContent::text(prompt));
            }
            output
        };
        match result {
            Ok(output) => Ok(with_prompt(output)),
            Err(frona_harness::ToolError::Feedback(output)) => {
                Err(frona_harness::ToolError::Feedback(with_prompt(output)))
            }
            result => result,
        }
    }
}
struct RequireInlineContinuation;
#[async_trait]
impl frona_harness::ToolCallHook for RequireInlineContinuation {
    async fn decide(
        &mut self,
        _: &rig_core::completion::message::ToolCall,
    ) -> Result<frona_harness::ToolDecision, frona_harness::ToolFailure> {
        Ok(frona_harness::ToolDecision::Execute)
    }
    async fn after(
        &mut self,
        _: &rig_core::completion::message::ToolCall,
        resolution: &frona_harness::ActionResolution,
    ) -> Result<(), ExecutionStop> {
        let effects = crate::tool::sdk_adapter::ToolEffects::read(resolution.output())
            .map_err(|e| ExecutionStop::Stopped(e.to_string()))?;
        if effects.is_some_and(|e| e.pending() || e.task_event.is_some()) {
            return Err(ExecutionStop::Stopped("This caller-owned run requires external continuation; its host output is recorded, but it will not resume as an ordinary chat".into()));
        }
        Ok(())
    }
}
