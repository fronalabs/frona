//! SurrealDB projection shared by ordinary chat and caller-owned harness runs.
//! Provider responses live on the message and tool actions live in tool-call rows.
//! The SDK retains the full event sequence in its in-memory conversation.
use super::{ChatService, Message, MessageRole, MessageStatus};
use crate::{
    chat::broadcast::EventSender,
    core::{error::AppError, repository::Repository},
    inference::{InferenceEvent, InferenceEventKind, tool_call::ToolCall},
    tool::{InferenceContext, active_chat, sdk_adapter::ToolEffects},
};
use async_trait::async_trait;
use frona_harness::{
    ActionPosition, ActionResolution, HistoryEvent, HistoryRecord, HistoryWriteError,
    HistoryWritePhase, HistoryWriter,
};
use rig_core::completion::{AssistantContent, message::ToolCall as ProviderCall};
use serde_json::Value;
use std::collections::HashSet;

/// Binds execution history to an agent-owned message for ordinary chats or custom runs.
/// This read-only binding is shared with runtime tools. History state belongs solely to
/// the writer; cloning this handle does not create another persistence cursor.
#[derive(Clone)]
pub struct MessageHistoryBinding {
    pub(crate) service: ChatService,
    pub(crate) user_id: String,
    pub(crate) message_id: String,
    chat_id: String,
    agent_id: String,
    first_turn: u32,
    events: EventSender,
}
impl std::fmt::Debug for MessageHistoryBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageHistoryBinding")
            .field("message_id", &self.message_id)
            .finish()
    }
}
impl MessageHistoryBinding {
    pub async fn new(
        service: &ChatService,
        ctx: &InferenceContext,
        message_id: &str,
    ) -> Result<Self, AppError> {
        let chat = active_chat(ctx)?;
        if chat.user_id != ctx.user.id || ctx.agent.user_id != ctx.user.id {
            return Err(AppError::Forbidden(
                "History requires the agent's own chat".into(),
            ));
        }
        let history = Self {
            service: service.clone(),
            user_id: ctx.user.id.clone(),
            message_id: message_id.into(),
            chat_id: chat.id.clone(),
            agent_id: ctx.agent.id.clone(),
            first_turn: next_provider_turn(service, message_id).await?,
            events: ctx.event_tx.clone(),
        };
        history.ensure_executing().await?;
        Ok(history)
    }
    pub fn with_events(mut self, events: EventSender) -> Self {
        self.events = events;
        self
    }
    pub fn into_writer(self) -> MessageHistoryWriter {
        MessageHistoryWriter {
            host: self,
            active_turn: None,
            text: String::new(),
        }
    }
    pub async fn message(&self) -> Result<Message, AppError> {
        self.service
            .get_message(&self.user_id, &self.message_id)
            .await
    }
    pub async fn ensure_executing(&self) -> Result<(), AppError> {
        let message = self.message().await?;
        if message.chat_id != self.chat_id
            || message.agent_id.as_deref() != Some(&self.agent_id)
            || message.role != MessageRole::Agent
            || message.status != Some(MessageStatus::Executing)
        {
            return Err(AppError::Forbidden(
                "Tool dispatch requires this agent's executing message in its active chat".into(),
            ));
        }
        Ok(())
    }
    fn turn(&self, response: usize) -> Result<u32, AppError> {
        self.first_turn
            .checked_add(
                u32::try_from(response)
                    .map_err(|_| AppError::Internal("Provider turn index exhausted".into()))?,
            )
            .ok_or_else(|| AppError::Internal("Provider turn index exhausted".into()))
    }
    pub fn call_id(&self, action: ActionPosition) -> Result<String, AppError> {
        Ok(format!(
            "{}-{}-{}",
            self.message_id,
            self.turn(action.response)?,
            action.action
        ))
    }
    pub async fn tool_record(&self, action: ActionPosition) -> Result<ToolCall, AppError> {
        self.service
            .tool_call_repo
            .find_by_id(&self.call_id(action)?)
            .await?
            .ok_or_else(|| AppError::NotFound("Recorded tool call not found".into()))
    }
}

/// Projects SDK history into the bound message and its canonical tool records.
#[derive(Debug)]
pub struct MessageHistoryWriter {
    pub(crate) host: MessageHistoryBinding,
    active_turn: Option<ActiveTurn>,
    pub(crate) text: String,
}
#[derive(Debug)]
struct ActiveTurn {
    response: usize,
    calls: Vec<ProviderCall>,
    started: HashSet<usize>,
    reasoning: Option<crate::chat::message::models::Reasoning>,
}

impl MessageHistoryWriter {
    async fn begin(&mut self, action: ActionPosition) -> Result<ToolCall, AppError> {
        let turn = self
            .active_turn
            .as_mut()
            .filter(|turn| turn.response == action.response)
            .ok_or_else(|| AppError::Internal("Unrecorded provider response".into()))?;
        if turn.started.contains(&action.action) {
            return self.host.tool_record(action).await;
        }
        let call = turn
            .calls
            .get(action.action)
            .ok_or_else(|| AppError::Internal("Unknown tool action".into()))?;
        let raw_arguments = frona_harness::inference::submission::call_arguments(&call.function);
        let (arguments, description) =
            crate::tool::sdk_adapter::project_arguments(call.function.name.as_str(), raw_arguments);
        let first = turn.started.is_empty();
        let record = ToolCall {
            id: self.host.call_id(action)?,
            chat_id: self.host.chat_id.clone(),
            message_id: self.host.message_id.clone(),
            turn: self.host.turn(action.response)?,
            provider_call_id: call.id.wire().into_owned(),
            provider_item_id: call
                .native
                .as_ref()
                .and_then(|n| n.item.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            signature: call
                .native
                .as_ref()
                .and_then(|n| n.item.get("thoughtSignature"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            name: call.function.name.to_string(),
            arguments,
            description,
            result: String::new(),
            success: false,
            duration_ms: 0,
            hitl: None,
            task_event: None,
            system_prompt: None,
            turn_text: (first && !self.text.is_empty()).then(|| self.text.clone()),
            turn_reasoning: if first { turn.reasoning.take() } else { None },
            created_at: chrono::Utc::now(),
        };
        self.host.service.tool_call_repo.create(&record).await?;
        turn.started.insert(action.action);
        self.host.events.send(InferenceEvent {
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
    async fn resolve(
        &mut self,
        action: ActionPosition,
        output: &frona_harness::ToolOutput,
        success: bool,
    ) -> Result<Value, AppError> {
        let mut record = self.begin(action).await?;
        let effects = ToolEffects::read(output)?;
        record.result = effects
            .as_ref()
            .map(|e| e.text.clone())
            .unwrap_or_else(|| output_text(output));
        record.success = effects.as_ref().map_or(success, |e| e.success);
        if let Some(effects) = &effects {
            record.duration_ms = effects.duration_ms;
            record.hitl = effects.hitl.clone();
            record.task_event = effects.task_event.clone();
            record.system_prompt = effects.system_prompt.clone();
        }
        self.host.service.tool_call_repo.update(&record).await?;
        if let Some(effects) = &effects
            && !effects.attachments.is_empty()
        {
            let mut message = self.host.message().await?;
            let count = message.attachments.len();
            crate::storage::models::extend_unique_attachments(
                &mut message.attachments,
                &effects.attachments,
            );
            if message.attachments.len() != count {
                self.host
                    .service
                    .message_repo
                    .db()
                    .query("UPDATE type::record('message', $id) SET attachments = $attachments")
                    .bind(("id", self.host.message_id.clone()))
                    .bind(("attachments", message.attachments))
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?
                    .check()
                    .map_err(|e| AppError::Database(e.to_string()))?;
            }
        }
        if !effects.as_ref().is_some_and(ToolEffects::pending) {
            self.host.events.send(InferenceEvent {
                kind: InferenceEventKind::ToolResult {
                    id: record.id,
                    name: record.name,
                    result: record.result,
                    success: record.success,
                },
            });
        }
        Ok(record.arguments)
    }
    async fn persist(&mut self, record: &HistoryRecord) -> Result<(), AppError> {
        match &record.event {
            HistoryEvent::Opened { .. } => self.host.ensure_executing().await?,
            HistoryEvent::Response {
                response, output, ..
            } => {
                let assistant = rig_core::message::AssistantMessage::new(output.content.clone())
                    .with_origin(output.origin.clone())
                    .with_stop(output.stop.clone());
                record_provider_turn(
                    &self.host.service,
                    &self.host.message_id,
                    self.host.turn(*response)?,
                    &assistant,
                )
                .await?;
                self.text = frona_harness::inference::provider::extract_text_from_choice(
                    &assistant.content,
                )
                .unwrap_or_default();
                let reasoning = crate::inference::tool_loop::extract_reasoning(&assistant.content);
                self.active_turn = Some(ActiveTurn {
                    response: *response,
                    reasoning,
                    calls: assistant
                        .content
                        .into_iter()
                        .filter_map(|content| match content {
                            AssistantContent::ToolCall(call) => Some(call),
                            _ => None,
                        })
                        .collect(),
                    started: HashSet::new(),
                });
            }
            HistoryEvent::ToolStarted { action, .. } => {
                self.begin(*action).await?;
            }
            HistoryEvent::ActionResolved { action, resolution } => {
                self.resolve(
                    *action,
                    resolution.output(),
                    matches!(resolution, ActionResolution::Result(_)),
                )
                .await?;
            }
            HistoryEvent::SubmissionCompleted {
                action,
                acknowledgement,
                ..
            } => {
                let arguments = self.resolve(*action, acknowledgement, true).await?;
                self.text = serde_json::to_string_pretty(&arguments)
                    .map_err(|e| AppError::Internal(e.to_string()))?;
            }
            HistoryEvent::Checkpoint { .. } => {
                return Err(AppError::Validation(
                    "Chat message storage does not support application checkpoints".into(),
                ));
            }
            // The chat driver owns context and terminal message lifecycle.
            HistoryEvent::User { .. }
            | HistoryEvent::Compacted { .. }
            | HistoryEvent::Finished { .. }
            | HistoryEvent::Stopped { .. } => {}
        }
        Ok(())
    }
}
#[async_trait]
impl HistoryWriter for MessageHistoryWriter {
    async fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        self.persist(record).await.map_err(|e| HistoryWriteError {
            phase: HistoryWritePhase::Append,
            cause: e.to_string(),
        })
    }
}
fn output_text(output: &frona_harness::ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|c| match c {
            frona_harness::ToolContent::Text(t) => Some(t.text.clone()),
            frona_harness::ToolContent::Json { value } => Some(value.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn next_provider_turn(service: &ChatService, message_id: &str) -> Result<u32, AppError> {
    let message = service
        .message_repo
        .find_by_id(message_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
    let mut last = service
        .get_tool_calls_by_message(message_id)
        .await?
        .into_iter()
        .map(|call| call.turn)
        .max();
    for key in message.provider_turns.keys() {
        let turn = key
            .parse::<u32>()
            .map_err(|_| AppError::Internal("Invalid persisted provider turn index".into()))?;
        last = Some(last.map_or(turn, |previous| previous.max(turn)));
    }
    last.map_or(Ok(0), |last| {
        last.checked_add(1)
            .ok_or_else(|| AppError::Internal("Provider turn index exhausted".into()))
    })
}

async fn record_provider_turn(
    service: &ChatService,
    message_id: &str,
    turn: u32,
    assistant: &rig_core::message::AssistantMessage,
) -> Result<(), AppError> {
    let mut message = service
        .message_repo
        .find_by_id(message_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Message not found".into()))?;
    let turns = &mut message.provider_turns;
    if turns.contains_key(&turn.to_string()) {
        return Err(AppError::Conflict("Provider turn already recorded".into()));
    }
    turns.insert(
        turn.to_string(),
        serde_json::to_value(assistant).map_err(|error| {
            AppError::Internal(format!("Cannot preserve provider turn: {error}"))
        })?,
    );
    service.message_repo.update(&message).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::models::Chat;
    #[tokio::test]
    async fn provider_turns_survive_resume_and_stale_finalization_without_public_exposure() {
        use rig_core::message::{AssistantContent, AssistantMessage, Origin, Reasoning};
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let fixture = crate::app_state_fixture::build(&db).await;
        let service = &fixture.state.chat_service;
        let now = chrono::Utc::now();
        let chat: Chat = serde_json::from_value(serde_json::json!({
            "id": "provider-turn-chat", "user_id": "user", "agent_id": "agent",
            "created_at": now, "updated_at": now,
        }))
        .unwrap();
        service.chat_repo.create(&chat).await.unwrap();
        let mut stale = Message::builder(&chat.id, MessageRole::Agent, "".into())
            .status(MessageStatus::Executing)
            .build();
        stale
            .metadata
            .insert("visible".into(), serde_json::json!("kept"));
        service.message_repo.create(&stale).await.unwrap();
        let first = AssistantMessage::new(vec![
            AssistantContent::Reasoning(Reasoning::new("private")).with_native(
                serde_json::json!({"type":"thinking","thinking":"private","signature":"opaque"}),
            ),
        ])
        .with_origin(Origin::new("anthropic.messages", "anthropic", "fixture"));
        assert_eq!(next_provider_turn(service, &stale.id).await.unwrap(), 0);
        record_provider_turn(service, &stale.id, 0, &first)
            .await
            .unwrap();
        assert!(matches!(
            record_provider_turn(service, &stale.id, 0, &first).await,
            Err(AppError::Conflict(_))
        ));
        assert_eq!(next_provider_turn(service, &stale.id).await.unwrap(), 1);
        service.save_updated_message(&stale).await.unwrap();
        let second = AssistantMessage::new(vec![AssistantContent::text("done")]);
        record_provider_turn(service, &stale.id, 1, &second)
            .await
            .unwrap();
        stale.content = "done".into();
        let public = service.complete_agent_message(stale.clone()).await.unwrap();
        assert!(!public.metadata.contains_key("provider_turns"));
        assert_eq!(public.metadata["visible"], "kept");
        let saved = service.find_message(&stale.id).await.unwrap().unwrap();
        let turns = &saved.provider_turns;
        assert_eq!(
            serde_json::from_value::<AssistantMessage>(turns["0"].clone()).unwrap(),
            first
        );
        assert_eq!(
            serde_json::from_value::<AssistantMessage>(turns["1"].clone()).unwrap(),
            second
        );
        assert_eq!(next_provider_turn(service, &stale.id).await.unwrap(), 2);
    }
}
