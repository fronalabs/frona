//! Server effects around the same action dispatcher used by portable loops.
//!
//! The caller records the raw model response in `Conversation` first. This
//! adapter projects UI metadata only for execution and canonical server rows.
//! Async server writes deliberately do not implement the synchronous SDK history
//! writer. A required write failure stops dispatch, retaining any started action.

use frona_harness as harness;
use frona_harness::{
    ActionResolution, Conversation, ExecutionControl, ExecutionStop, PendingAction, Tool,
    ToolContext, ToolDispatchError, ToolError, ToolResult,
};
use std::time::Instant;

use async_trait::async_trait;
use rig_core::completion::{message::ToolCall, request::ToolDefinition};
use serde_json::Value;

use super::{
    InferenceContext, ToolOutput, active_chat,
    registry::AgentToolRegistry,
    sdk_adapter::{content_result, legacy_error},
};
use crate::{
    chat::{
        message::models::{MessageRole, MessageStatus, Reasoning},
        service::ChatService,
    },
    core::{error::AppError, metrics},
    inference::{
        InferenceEvent, InferenceEventKind,
        tool_call::{ToolCallResponse, ToolStatus},
    },
    storage::Attachment,
};

/// One completed server invocation. `PersistentToolOutcome::Pending` requires
/// pausing before another model request. The invocation has finished creating the
/// prompt/callback request; it is NOT an interrupted SDK Started action.
///
/// Resume uses the existing HITL resolver and per-message barrier, then reloads
/// `PersistentConversation` from canonical rows containing the actual answer.
/// Never recover a pending HITL by retrying this invocation or send its temporary
/// output to a model while waiting. Task events are handed to the task executor.
#[derive(Debug)]
pub struct PersistentToolResult {
    pub record: ToolCallResponse,
    pub resolution: ActionResolution,
    pub attachments: Vec<Attachment>,
}

/// Host control flow after one invocation. Complete the current proposed batch,
/// then prefer Pending over Task over Continue, matching existing server behavior.
#[derive(Debug)]
pub enum PersistentToolOutcome {
    Continue(PersistentToolResult),
    Pending(PersistentToolResult),
    Task(PersistentToolResult),
}

impl PersistentToolOutcome {
    pub fn result(&self) -> &PersistentToolResult {
        match self {
            Self::Continue(result) | Self::Pending(result) | Self::Task(result) => result,
        }
    }
}

/// Bound to a real executing Agent message, checked against canonical ownership.
/// Construct one per provider turn so text/reasoning are stamped on its first row.
pub struct PersistentToolDispatcher<'a> {
    service: &'a ChatService,
    registry: &'a AgentToolRegistry,
    context: &'a InferenceContext,
    events: crate::chat::broadcast::EventSender,
    message_id: String,
    turn: u32,
    turn_text: Option<String>,
    reasoning: Option<Reasoning>,
    metadata_saved: bool,
}

impl<'a> PersistentToolDispatcher<'a> {
    pub async fn new(
        service: &'a ChatService,
        registry: &'a AgentToolRegistry,
        context: &'a InferenceContext,
        message_id: &str,
        turn: u32,
        turn_text: Option<String>,
        reasoning: Option<Reasoning>,
    ) -> Result<Self, AppError> {
        let chat = active_chat(context)?;
        let message = service.get_message(&context.user.id, message_id).await?;
        if message.chat_id != chat.id
            || chat.user_id != context.user.id
            || context.agent.user_id != context.user.id
            || message.agent_id.as_deref() != Some(context.agent.id.as_str())
            || message.role != MessageRole::Agent
            || message.status != Some(MessageStatus::Executing)
        {
            return Err(AppError::Forbidden(
                "Tool dispatch requires this agent's executing message in its active chat".into(),
            ));
        }
        Ok(Self {
            service,
            registry,
            context,
            events: context.event_tx.clone(),
            message_id: message_id.into(),
            turn,
            turn_text,
            reasoning,
            metadata_saved: false,
        })
    }

    /// Preserve an existing runtime caller's explicit lifecycle event sink.
    pub fn with_events(mut self, events: crate::chat::broadcast::EventSender) -> Self {
        self.events = events;
        self
    }

    /// Dispatch a proposal with fresh shared admission. Authorization remains in
    /// the runtime registry's AuthorizedTool, including its controlled path.
    pub async fn execute(
        &mut self,
        conversation: &mut Conversation,
        action: &PendingAction,
        execution: ExecutionControl<'_>,
    ) -> Result<PersistentToolOutcome, ToolDispatchError> {
        let proposal = conversation.action_proposal(action)?;
        let call = proposal.call.clone();
        let name = &call.function.name;
        let offered = proposal.offered_tools.iter().any(|tool| tool.name == *name);
        let definition = self
            .registry
            .definitions()
            .iter()
            .find(|tool| tool.id == *name);
        if !offered || definition.is_none() {
            let feedback = if !offered {
                format!("Tool '{name}' was not offered for this request.")
            } else {
                format!("Tool '{name}' is not registered.")
            };
            return self.reject(conversation, action, execution, feedback).await;
        }
        conversation.tool_proposal(action)?;
        let definition = definition.expect("checked registered tool");
        let mut returned = None;
        let mut tools = harness::ToolRegistry::default();
        tools
            .register(PersistentInvocation {
                definition: ToolDefinition {
                    name: definition.id.clone().try_into().map_err(|_| {
                        ToolDispatchError::ToolHook("tool name cannot be empty".into())
                    })?,
                    description: definition.description.clone(),
                    parameters: definition.parameters.clone(),
                    annotations: Default::default(),
                },
                dispatcher: self,
                call,
                returned: &mut returned,
            })
            .map_err(|error| ToolDispatchError::ToolHook(error.to_string()))?;
        let resolution = conversation
            .execute_tool(action, &mut tools, execution)
            .await;
        drop(tools);
        let resolution = resolution?;
        let (record, attachments, pending_external) =
            returned.expect("successful registered dispatch returns a server record");
        let has_task = record.task_event.is_some();
        let result = PersistentToolResult {
            record,
            resolution,
            attachments,
        };
        Ok(if pending_external {
            PersistentToolOutcome::Pending(result)
        } else if has_task {
            PersistentToolOutcome::Task(result)
        } else {
            PersistentToolOutcome::Continue(result)
        })
    }

    /// Persist paired rejection without invoking a tool or consuming allowance.
    pub async fn reject(
        &mut self,
        conversation: &mut Conversation,
        action: &PendingAction,
        execution: ExecutionControl<'_>,
        feedback: String,
    ) -> Result<PersistentToolOutcome, ToolDispatchError> {
        conversation.action_proposal(action)?;
        if let Err(error) = execution.check_cancellation() {
            return Err(conversation
                .stop_execution(error.into_execution_stop().expect("terminal cancellation")));
        }
        let call = conversation.action(action)?.call.clone();
        let result = async {
            let mut record = self.begin(&call).await?;
            self.finish(&mut record, &ToolOutput::error(&feedback), 0)
                .await?;
            Ok::<_, AppError>(record)
        }
        .await;
        let record = match result {
            Ok(record) => record,
            Err(error) => {
                return Err(conversation.stop_execution(ExecutionStop::Storage(Box::new(error))));
            }
        };
        // Required writes retain precedence and their canonical evidence. A
        // cancellation arriving while they ran must still prevent continuation.
        if let Err(error) = execution.check_cancellation() {
            return Err(conversation
                .stop_execution(error.into_execution_stop().expect("terminal cancellation")));
        }
        let resolution = if conversation.action(action)?.kind == harness::ActionKind::Submission {
            conversation
                .revise_submission(action, feedback.clone())
                .map_err(|error| match error {
                    harness::SubmissionError::Conversation(error) => ToolDispatchError::from(error),
                    error => ToolDispatchError::ToolHook(error.to_string()),
                })?;
            ActionResolution::Rejected(harness::ToolOutput::text(feedback))
        } else {
            conversation.reject_tool(action, harness::ToolOutput::text(feedback))?
        };
        Ok(PersistentToolOutcome::Continue(PersistentToolResult {
            record: record.into(),
            resolution,
            attachments: vec![],
        }))
    }

    async fn begin(
        &mut self,
        call: &ToolCall,
    ) -> Result<crate::inference::tool_call::ToolCall, AppError> {
        let (arguments, description) = project_arguments(
            &harness::inference::submission::call_arguments(&call.function),
        );
        let record = self
            .service
            .begin_tool_call(
                &crate::core::repository::new_id(),
                &active_chat(self.context)?.id,
                &self.message_id,
                self.turn,
                &call.id.wire(),
                call.native
                    .as_ref()
                    .and_then(|native| native.item.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                call.native
                    .as_ref()
                    .and_then(|native| native.item.get("thoughtSignature"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                &call.function.name,
                &arguments,
                description,
                if self.metadata_saved {
                    None
                } else {
                    self.turn_text.clone()
                },
                if self.metadata_saved {
                    None
                } else {
                    self.reasoning.clone()
                },
            )
            .await?;
        self.metadata_saved = true;
        self.events.send(InferenceEvent {
            kind: InferenceEventKind::ToolCall {
                id: record.id.clone(),
                provider_call_id: record.provider_call_id.clone(),
                name: record.name.clone(),
                arguments: record.arguments.clone(),
                description: record.description.clone(),
            },
        });
        Ok(record)
    }

    async fn finish(
        &self,
        record: &mut crate::inference::tool_call::ToolCall,
        output: &ToolOutput,
        duration_ms: u64,
    ) -> Result<(), AppError> {
        let text = output.text_content().to_owned();
        let system_prompt = output.system_prompt().map(str::to_owned);
        self.service
            .finish_tool_call(
                &record.id,
                text.clone(),
                output.is_success(),
                duration_ms,
                system_prompt.clone(),
            )
            .await?;
        if let Some(hitl) = output.hitl() {
            self.service.set_hitl(&record.id, hitl.clone()).await?;
        }
        if let Some(event) = output.task_event() {
            self.service
                .set_task_event(&record.id, event.clone())
                .await?;
        }
        record.result = text.clone();
        record.success = output.is_success();
        record.duration_ms = duration_ms;
        record.hitl = output.hitl().cloned();
        record.task_event = output.task_event().cloned();
        record.system_prompt = system_prompt;
        if !pending(output) {
            self.events.send(InferenceEvent {
                kind: InferenceEventKind::ToolResult {
                    name: record.name.clone(),
                    result: text,
                    success: record.success,
                },
            });
        }
        Ok(())
    }
}

/// UI description does not belong to tool arguments, but the original provider
/// response stays untouched in SDK Conversation for inspection and persistence.
fn project_arguments(arguments: &Value) -> (Value, Option<String>) {
    let mut arguments = arguments.clone();
    let description = arguments
        .as_object_mut()
        .and_then(|object| object.remove("description"))
        .and_then(|value| value.as_str().map(str::to_owned));
    (arguments, description)
}

fn pending(output: &ToolOutput) -> bool {
    output.is_pending_external()
        || output
            .hitl()
            .is_some_and(|hitl| hitl.status == ToolStatus::Pending)
}

type Returned = (ToolCallResponse, Vec<Attachment>, bool);
struct PersistentInvocation<'a, 'b> {
    definition: ToolDefinition,
    dispatcher: &'b mut PersistentToolDispatcher<'a>,
    call: ToolCall,
    returned: &'b mut Option<Returned>,
}

#[async_trait]
impl Tool for PersistentInvocation<'_, '_> {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn call(&mut self, _arguments: Value, control: &ToolContext<'_>) -> ToolResult {
        let record = self.dispatcher.begin(&self.call).await;
        let mut record = record.map_err(|error| ExecutionStop::Storage(Box::new(error)))?;
        // Cancellation may arrive during an async begin write. No external effect
        // starts after that cancellation; its canonical unfinished row remains.
        control.execution()?.check_cancellation()?;
        let started = Instant::now();
        let result = self
            .dispatcher
            .registry
            .execute_controlled(
                &self.definition.name,
                record.arguments.clone(),
                self.dispatcher.context,
                control,
            )
            .await;
        let output = match result {
            Ok(output) => output,
            Err(error) => match legacy_error(error) {
                ToolError::Stop(stop) => return Err(stop.into()),
                ToolError::Feedback(_) => unreachable!("legacy_error produces error or stop"),
                ToolError::Failed(error) => ToolOutput::error(format!("Error: {error}")),
            },
        };
        let duration = started.elapsed();
        metrics::record_tool_call(
            &record.name,
            &self.dispatcher.context.user.id,
            &self.dispatcher.context.agent.id,
            duration,
            if output.is_success() {
                "success"
            } else {
                "error"
            },
        );
        self.dispatcher
            .finish(&mut record, &output, duration.as_millis() as u64)
            .await
            .map_err(|error| ExecutionStop::Storage(Box::new(error)))?;
        *self.returned = Some((
            record.into(),
            output.attachments().to_vec(),
            pending(&output),
        ));
        content_result(&output)
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
                project_arguments(&harness::inference::submission::call_arguments(
                    &call.function,
                ))
                .0,
            );
        }
    }
}
