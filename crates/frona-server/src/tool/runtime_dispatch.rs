//! Runtime tool execution and host outcomes. Persistence belongs exclusively to
//! the conversation's attached MessageHistoryWriter.
use super::{
    InferenceContext, ToolOutput,
    registry::AgentToolRegistry,
    sdk_adapter::{ToolEffects, content_result, legacy_error, project_arguments},
};
use crate::{
    chat::service::history::MessageHistoryBinding,
    core::{error::AppError, metrics},
    inference::tool_call::{TaskEvent, ToolCallResponse},
    storage::Attachment,
};
use async_trait::async_trait;
use frona_harness::{
    self as harness, ActionResolution, Conversation, ExecutionControl, ExecutionStop,
    PendingAction, Tool, ToolContext, ToolDispatchError, ToolError, ToolResult,
};
use rig_core::completion::request::ToolDefinition;
use serde_json::Value;

#[derive(Debug)]
pub struct RuntimeToolOutcome {
    pub pending: Option<ToolCallResponse>,
    pub task_event: Option<TaskEvent>,
    pub system_prompt: Option<String>,
    pub resolution: ActionResolution,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct RuntimeToolBatchError {
    #[source]
    pub error: ToolDispatchError,
    pub attachments: Vec<Attachment>,
}
impl From<ToolDispatchError> for RuntimeToolBatchError {
    fn from(error: ToolDispatchError) -> Self {
        Self {
            error,
            attachments: vec![],
        }
    }
}
impl From<harness::ConversationError> for RuntimeToolBatchError {
    fn from(error: harness::ConversationError) -> Self {
        ToolDispatchError::from(error).into()
    }
}

pub struct RuntimeToolDispatcher<'a> {
    history: MessageHistoryBinding,
    registry: &'a AgentToolRegistry,
    context: &'a InferenceContext,
}
impl<'a> RuntimeToolDispatcher<'a> {
    pub async fn new(
        history: MessageHistoryBinding,
        registry: &'a AgentToolRegistry,
        context: &'a InferenceContext,
    ) -> Result<Self, AppError> {
        let message = history.message().await?;
        if message.agent_id.as_deref() != Some(&context.agent.id)
            || history.user_id != context.user.id
            || context.chat.as_ref().map(|c| c.id.as_str()) != Some(message.chat_id.as_str())
        {
            return Err(AppError::Forbidden(
                "Tool dispatch requires the owning context".into(),
            ));
        }
        history.ensure_executing().await?;
        Ok(Self {
            history,
            registry,
            context,
        })
    }
    async fn outcome_at(
        &self,
        position: harness::ActionPosition,
        resolution: ActionResolution,
    ) -> Result<RuntimeToolOutcome, ToolDispatchError> {
        let effects = ToolEffects::read(resolution.output())
            .map_err(|e| ExecutionStop::Storage(Box::new(e)))?;
        let pending = if effects.as_ref().is_some_and(ToolEffects::pending) {
            Some(
                self.history
                    .tool_record(position)
                    .await
                    .map_err(|e| ExecutionStop::Storage(Box::new(e)))?
                    .into(),
            )
        } else {
            None
        };
        let (attachments, task_event, system_prompt) = effects
            .map(|effects| {
                (
                    effects.attachments,
                    effects.task_event,
                    effects.system_prompt,
                )
            })
            .unwrap_or_default();
        Ok(RuntimeToolOutcome {
            pending,
            task_event,
            system_prompt,
            attachments,
            resolution,
        })
    }
    pub async fn execute(
        &mut self,
        conversation: &mut Conversation,
        action: &PendingAction,
        execution: ExecutionControl<'_>,
    ) -> Result<RuntimeToolOutcome, ToolDispatchError> {
        let mut result = self
            .execute_batch(
                conversation,
                std::slice::from_ref(action),
                execution,
                std::num::NonZeroUsize::new(1).unwrap(),
            )
            .await
            .map_err(|e| e.error)?;
        Ok(result.remove(0))
    }
    pub async fn execute_batch(
        &mut self,
        conversation: &mut Conversation,
        actions: &[PendingAction],
        execution: ExecutionControl<'_>,
        maximum: std::num::NonZeroUsize,
    ) -> Result<Vec<RuntimeToolOutcome>, RuntimeToolBatchError> {
        let mut definitions = Vec::new();
        let mut positions = Vec::new();
        for action in actions {
            let proposal = conversation.action_proposal(action)?;
            positions.push(conversation.action_position(action)?);
            let definition = self
                .registry
                .definitions()
                .iter()
                .find(|d| d.id == proposal.call.function.name.as_str());
            if !proposal
                .offered_tools
                .iter()
                .any(|d| d.name == proposal.call.function.name)
                || definition.is_none()
            {
                let mut results = Vec::new();
                for action in actions {
                    let proposal = conversation.action_proposal(action)?;
                    let name = proposal.call.function.name.to_string();
                    let offered = proposal
                        .offered_tools
                        .iter()
                        .any(|d| d.name == proposal.call.function.name);
                    let registered = self.registry.definitions().iter().any(|d| d.id == name);
                    let result = if offered && registered {
                        Box::pin(self.execute(conversation, action, execution)).await
                    } else {
                        let feedback = if !offered {
                            format!("Tool '{name}' was not offered for this request.")
                        } else {
                            format!("Tool '{name}' is not registered.")
                        };
                        self.reject(conversation, action, execution, feedback).await
                    };
                    match result {
                        Ok(result) => results.push(result),
                        Err(error) => {
                            return Err(RuntimeToolBatchError {
                                error,
                                attachments: results
                                    .iter()
                                    .flat_map(|r: &RuntimeToolOutcome| r.attachments.clone())
                                    .collect(),
                            });
                        }
                    }
                }
                return Ok(results);
            }
            conversation.tool_proposal(action)?;
            definitions.push(definition.unwrap().clone());
        }
        let mut calls = Vec::new();
        for (action, definition) in actions.iter().zip(definitions) {
            calls.push((
                action,
                Box::new(RuntimeInvocation {
                    definition: ToolDefinition {
                        name: definition
                            .id
                            .try_into()
                            .map_err(|_| ToolDispatchError::ToolHook("empty tool name".into()))?,
                        description: definition.description,
                        parameters: definition.parameters,
                        annotations: Default::default(),
                    },
                    history: &self.history,
                    registry: self.registry,
                    context: self.context,
                }) as Box<dyn Tool>,
            ));
        }
        let resolutions = conversation
            .execute_tool_instances(&mut calls, execution, maximum)
            .await;
        drop(calls);
        let resolutions = resolutions.map_err(|error| RuntimeToolBatchError {
            error,
            attachments: positions
                .iter()
                .flat_map(|p| {
                    conversation.responses()[p.response].actions()[p.action]
                        .resolution()
                        .and_then(|r| ToolEffects::read(r.output()).ok().flatten())
                        .map_or_else(Vec::new, |e| e.attachments)
                })
                .collect(),
        })?;
        let mut results = Vec::new();
        for (position, resolution) in positions.into_iter().zip(resolutions) {
            results.push(self.outcome_at(position, resolution).await?);
        }
        Ok(results)
    }
    pub async fn reject(
        &mut self,
        conversation: &mut Conversation,
        action: &PendingAction,
        execution: ExecutionControl<'_>,
        feedback: String,
    ) -> Result<RuntimeToolOutcome, ToolDispatchError> {
        conversation.action_proposal(action)?;
        let position = conversation.action_position(action)?;
        if let Err(e) = execution.check_cancellation() {
            return Err(conversation
                .stop_execution(e.into_execution_stop().expect("cancellation"))
                .await);
        }
        let resolution = if conversation.action(action)?.kind == harness::ActionKind::Submission {
            conversation
                .revise_submission(action, feedback.clone())
                .await
                .map_err(|e| match e {
                    harness::SubmissionError::Conversation(e) => ToolDispatchError::from(e),
                    e => ToolDispatchError::ToolHook(e.to_string()),
                })?;
            ActionResolution::Rejected(harness::ToolOutput::text(feedback))
        } else {
            conversation
                .reject_tool(action, harness::ToolOutput::text(feedback))
                .await?
        };
        if let Err(e) = execution.check_cancellation() {
            return Err(conversation
                .stop_execution(e.into_execution_stop().expect("cancellation"))
                .await);
        }
        self.outcome_at(position, resolution).await
    }
}
struct RuntimeInvocation<'a> {
    definition: ToolDefinition,
    history: &'a MessageHistoryBinding,
    registry: &'a AgentToolRegistry,
    context: &'a InferenceContext,
}
#[async_trait]
impl Tool for RuntimeInvocation<'_> {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn call(&mut self, arguments: Value, control: &ToolContext<'_>) -> ToolResult {
        invoke_runtime_tool(
            self.history,
            self.registry,
            self.context,
            &self.definition.name,
            arguments,
            control,
        )
        .await
    }
}
pub async fn invoke_runtime_tool(
    history: &MessageHistoryBinding,
    registry: &AgentToolRegistry,
    ctx: &InferenceContext,
    name: &str,
    arguments: Value,
    control: &ToolContext<'_>,
) -> ToolResult {
    history.ensure_executing().await.map_err(legacy_error)?;
    let started = std::time::Instant::now();
    let output = match registry
        .execute_controlled(name, project_arguments(name, arguments).0, ctx, control)
        .await
    {
        Ok(output) => output,
        Err(e) => match legacy_error(e) {
            ToolError::Stop(s) => return Err(s.into()),
            ToolError::Failed(e) => ToolOutput::error(format!("Error: {e}")),
            ToolError::Feedback(_) => unreachable!(),
        },
    };
    let duration = started.elapsed();
    metrics::record_tool_call(
        name,
        &ctx.user.id,
        &ctx.agent.id,
        duration,
        if output.is_success() {
            "success"
        } else {
            "error"
        },
    );
    let metadata = serde_json::json!({"frona_effects":ToolEffects::from_output(&output,duration.as_millis() as u64)});
    match content_result(&output) {
        Ok(output) => Ok(output.with_metadata(metadata)),
        Err(ToolError::Feedback(output)) => {
            Err(ToolError::Feedback(output.with_metadata(metadata)))
        }
        error => error,
    }
}
/// Native server request projection for live SDK messages. Apply this only to
/// the current SDK suffix; PersistentConversation's historical prefix is already
/// frozen. Raw SDK history stays untouched, including description and image data.
pub fn project_messages(
    messages: &[rig_core::completion::Message],
) -> Vec<rig_core::completion::Message> {
    use rig_core::completion::{Message, message::UserContent};
    messages
        .iter()
        .cloned()
        .map(|message| match message {
            Message::Assistant(mut assistant) => {
                project_assistant(&mut assistant);
                Message::Assistant(assistant)
            }
            Message::User { content } => {
                let mut projected = Vec::new();
                for item in content {
                    match item {
                        UserContent::ToolResult(mut result) => {
                            let mut images = Vec::new();
                            result.content.retain(|content| {
                                if let harness::ToolContent::Image(image) = content {
                                    images.push(UserContent::Image(image.clone()));
                                    false
                                } else {
                                    true
                                }
                            });
                            projected.push(UserContent::ToolResult(result));
                            projected.extend(images);
                        }
                        item => projected.push(item),
                    }
                }
                Message::User { content: projected }
            }
            message => message,
        })
        .collect()
}

/// Apply the same argument projection to live and restored assistant turns.
/// The caller passes a view; the original recorded provider output stays intact.
pub(crate) fn project_assistant(assistant: &mut rig_core::message::AssistantMessage) {
    use rig_core::message::AssistantContent;
    for item in &mut assistant.content {
        if let AssistantContent::ToolCall(call) = item {
            call.function = rig_core::message::ToolFunction::new(
                call.function.name.clone(),
                project_arguments(
                    call.function.name.as_str(),
                    harness::inference::submission::call_arguments(&call.function),
                )
                .0,
            );
        }
    }
}
