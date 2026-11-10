use async_trait::async_trait;
use base64::Engine;
use rig_core::completion::message::{
    AssistantMessage, CallId, DocumentSourceKind, ImageMediaType, MimeType, ToolResult,
    ToolResultContent, UserContent,
};
use rig_core::completion::{AssistantContent, Message as RigMessage};

use std::collections::HashMap;

use crate::agent::service::AgentService;
use crate::auth::UserService;
use crate::chat::message::models::{Message, MessageEvent, MessageRole, MessageStatus};
use crate::inference::tool_call::ToolCall;
use crate::storage::{Attachment, StorageService, VirtualPath, is_image_content_type};

use frona_harness::inference::{ModelConfig, compaction::summary_message};

pub struct ConversationContext {
    pub agent_id: String,
    pub model_config: ModelConfig,
    pub user_id: String,
}

/// Native model history and the persisted-row boundaries that produced it.
/// Zero-output rows still have an end offset; one assistant row owns all of its
/// tool calls/results. The mapping is transient and never stored in the database.
#[derive(Clone, Debug)]
pub struct ModelConversationHistory {
    pub history: Vec<RigMessage>,
    pub row_ids: Vec<String>,
    pub row_ends: Vec<usize>,
    pub summary: Option<String>,
    pub summary_end: usize,
}

impl ModelConversationHistory {
    pub(crate) fn row_range(&self, row: usize) -> std::ops::Range<usize> {
        let start = row
            .checked_sub(1)
            .map_or(self.summary_end, |i| self.row_ends[i]);
        start..self.row_ends[row]
    }

    pub(crate) fn validate_source(
        &self,
        messages: &[Message],
        summary: Option<&str>,
    ) -> Result<(), crate::core::error::AppError> {
        if !self
            .row_ids
            .iter()
            .eq(messages.iter().map(|message| &message.id))
            || self.summary.as_deref() != summary
            || self.row_ends.len() != messages.len()
            || self.summary_end != usize::from(summary.is_some_and(|text| !text.trim().is_empty()))
            || self.summary_end > self.history.len()
            || self
                .row_ends
                .iter()
                .try_fold(self.summary_end, |previous, &end| {
                    (end >= previous && end <= self.history.len()).then_some(end)
                })
                .is_none()
        {
            return Err(crate::core::error::AppError::Conflict(
                "Model conversation history no longer matches the stored compaction source".into(),
            ));
        }
        Ok(())
    }

    /// Replace whole stored rows, retaining their native tail and injected messages.
    pub(crate) fn replace_prefix(
        &mut self,
        rows: usize,
        summary: &str,
        context: Option<&str>,
    ) -> Result<(), crate::core::error::AppError> {
        let prefix_end = self.row_range(rows - 1).end;
        self.history = frona_harness::HistoryView::compact_persisted_prefix(
            &self.history,
            prefix_end,
            summary_message(summary, context),
        )
        .map_err(|error| crate::core::error::AppError::Validation(error.to_string()))?
        .messages();
        self.row_ids.drain(..rows);
        self.row_ends.drain(..rows);
        for end in &mut self.row_ends {
            *end = *end - prefix_end + 1;
        }
        self.summary = Some(summary.to_owned());
        self.summary_end = 1;
        Ok(())
    }

    /// Include the frozen agent context in the leading summary message so both
    /// remain directly after the system prompt, including during history trimming.
    pub fn set_summary_context(&mut self, context: Option<&str>) {
        if let Some(summary) = self.summary.as_deref().filter(|s| !s.trim().is_empty()) {
            self.history[0] = summary_message(summary, context);
        }
    }

    fn new(messages: &[Message], summary: Option<&str>) -> Self {
        let mut history = Vec::new();
        if let Some(summary) = summary.filter(|s| !s.trim().is_empty()) {
            history.push(summary_message(summary, None));
        }
        Self {
            summary_end: history.len(),
            history,
            row_ids: messages.iter().map(|m| m.id.clone()).collect(),
            row_ends: Vec::with_capacity(messages.len()),
            summary: summary.map(str::to_owned),
        }
    }
}

#[async_trait]
pub trait ConversationBuilder: Send + Sync {
    /// `summary`: rolling summary of compacted-away earlier messages, prepended
    /// as a leading message by chat builders; `None` when there is no summary.
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> ModelConversationHistory;

    /// Turn-scoped system prompt appended to the agent's main system prompt.
    fn system_prompt(&self) -> Option<String> {
        None
    }
}

pub struct DefaultConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
}

#[async_trait]
impl ConversationBuilder for DefaultConversationBuilder {
    // NOTE: fallback models reuse this history - messages are not rebuilt per model.
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> ModelConversationHistory {
        let te_map = group_tool_calls_by_message(tool_calls);
        let mut model_history = ModelConversationHistory::new(messages, summary);
        let result = &mut model_history.history;
        for stored in messages {
            let mut projected = stored.clone();
            projected.content = stored.inference_content().to_owned();
            let msg = &projected;
            match msg.role {
                MessageRole::User | MessageRole::Contact => {
                    // Providers reject empty content blocks. Skip + warn.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::warn!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            role = ?msg.role,
                            "skipping empty user-side message in chat history (upstream produced a payload with no content and no attachments)",
                        );
                        model_history.row_ends.push(result.len());
                        continue;
                    }
                    result.push(
                        build_user_message(
                            &msg.content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::TaskCompletion => {
                    // Event-only rows (no summary) are expected; the event payload still renders in UI.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::debug!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            "skipping empty task-completion message in chat history (event-only row with no body)",
                        );
                        model_history.row_ends.push(result.len());
                        continue;
                    }
                    let content = task_completion_content(msg);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::LiveCall => {
                    let content = format!("[LIVE_CALL] {}", msg.content);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::Agent => {
                    let other_handle =
                        resolve_other_agent_handle(msg, &ctx.agent_id, &self.agent_service).await;
                    if let Some(tes) = te_map.get(&msg.id) {
                        convert_agent_with_tool_calls(
                            msg,
                            tes,
                            &ctx.agent_id,
                            other_handle.as_deref(),
                            result,
                        );
                    } else if let Some(m) =
                        convert_agent_message(msg, &ctx.agent_id, other_handle.as_deref())
                    {
                        result.push(m);
                    }
                }
                MessageRole::System => {
                    if !msg.content.is_empty() {
                        result.push(RigMessage::user(&msg.content));
                    }
                }
            };
            model_history.row_ends.push(result.len());
        }
        attach_legacy_origins(&mut model_history, &ctx.model_config);
        model_history
    }
}

pub struct TaskConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
    pub continuation_prompt: Option<String>,
}

#[async_trait]
impl ConversationBuilder for TaskConversationBuilder {
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> ModelConversationHistory {
        let te_map = group_tool_calls_by_message(tool_calls);
        let mut model_history = ModelConversationHistory::new(messages, summary);
        let result = &mut model_history.history;
        // Task execution creates the initial instruction as the first stored
        // Agent row. A summary already covers that row; retained Agent rows
        // must keep their normal assistant/tool representation on reload.
        let mut instruction_wrapped = summary.is_some();
        for stored in messages {
            let mut projected = stored.clone();
            projected.content = stored.inference_content().to_owned();
            let msg = &projected;
            match msg.role {
                MessageRole::User | MessageRole::Contact => {
                    // Providers reject empty content blocks. Skip + warn.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::warn!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            role = ?msg.role,
                            "skipping empty user-side message in chat history (upstream produced a payload with no content and no attachments)",
                        );
                        model_history.row_ends.push(result.len());
                        continue;
                    }
                    result.push(
                        build_user_message(
                            &msg.content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::TaskCompletion => {
                    // Event-only rows (no summary) are expected; the event payload still renders in UI.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::debug!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            "skipping empty task-completion message in chat history (event-only row with no body)",
                        );
                        model_history.row_ends.push(result.len());
                        continue;
                    }
                    let content = task_completion_content(msg);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::LiveCall => {
                    let content = format!("[LIVE_CALL] {}", msg.content);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::Agent => {
                    if !instruction_wrapped {
                        instruction_wrapped = true;
                        let content = format!("<task>\n{}\n</task>", msg.content);
                        result.push(RigMessage::user(&content));
                    } else {
                        let other_handle =
                            resolve_other_agent_handle(msg, &ctx.agent_id, &self.agent_service)
                                .await;
                        if let Some(tes) = te_map.get(&msg.id) {
                            convert_agent_with_tool_calls(
                                msg,
                                tes,
                                &ctx.agent_id,
                                other_handle.as_deref(),
                                result,
                            );
                        } else if let Some(m) =
                            convert_agent_message(msg, &ctx.agent_id, other_handle.as_deref())
                        {
                            result.push(m);
                        }
                    }
                }
                MessageRole::System => {
                    if !msg.content.is_empty() {
                        result.push(RigMessage::user(&msg.content));
                    }
                }
            };
            model_history.row_ends.push(result.len());
        }
        if let Some(prompt) = &self.continuation_prompt
            && !prompt.is_empty()
        {
            result.push(RigMessage::user(prompt));
        }
        attach_legacy_origins(&mut model_history, &ctx.model_config);
        model_history
    }
}

/// The injected heartbeat turn is never written to the message table -
/// it exists only inside the inference request.
pub struct HeartbeatConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
    pub continuation_prompt: Option<String>,
    pub heartbeat_content: String,
}

#[async_trait]
impl ConversationBuilder for HeartbeatConversationBuilder {
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> ModelConversationHistory {
        let default = DefaultConversationBuilder {
            user_service: self.user_service.clone(),
            storage_service: self.storage_service.clone(),
            agent_service: self.agent_service.clone(),
        };
        let mut model_history = default.build(messages, tool_calls, ctx, summary).await;

        let mut injected = String::new();
        if let Some(prompt) = &self.continuation_prompt
            && !prompt.is_empty()
        {
            injected.push_str("<heartbeat_instructions>\n");
            injected.push_str(prompt);
            injected.push_str("\n</heartbeat_instructions>\n\n");
        }
        injected.push_str("<heartbeat_checklist file=\"HEARTBEAT.md\">\n");
        injected.push_str(&self.heartbeat_content);
        injected.push_str("\n</heartbeat_checklist>");
        model_history.history.push(RigMessage::user(&injected));
        attach_legacy_origins(&mut model_history, &ctx.model_config);
        model_history
    }
}

pub struct ChannelConversationBuilder {
    pub user_service: UserService,
    pub storage_service: StorageService,
    pub agent_service: AgentService,
    pub channel: String,
    pub sender: Option<String>,
    pub inbound_prompt: Option<String>,
}

#[async_trait]
impl ConversationBuilder for ChannelConversationBuilder {
    async fn build(
        &self,
        messages: &[Message],
        tool_calls: &[ToolCall],
        ctx: &ConversationContext,
        summary: Option<&str>,
    ) -> ModelConversationHistory {
        let te_map = group_tool_calls_by_message(tool_calls);
        let mut model_history = ModelConversationHistory::new(messages, summary);
        let result = &mut model_history.history;
        for stored in messages {
            let mut projected = stored.clone();
            projected.content = stored.inference_content().to_owned();
            let msg = &projected;
            match msg.role {
                MessageRole::User | MessageRole::Contact => {
                    // Providers reject empty content blocks. Skip + warn.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::warn!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            role = ?msg.role,
                            "skipping empty user-side message in chat history (upstream produced a payload with no content and no attachments)",
                        );
                        model_history.row_ends.push(result.len());
                        continue;
                    }
                    result.push(
                        build_user_message(
                            &msg.content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::TaskCompletion => {
                    // Event-only rows (no summary) are expected; the event payload still renders in UI.
                    if msg.content.trim().is_empty() && msg.attachments.is_empty() {
                        tracing::debug!(
                            msg_id = %msg.id,
                            chat_id = %msg.chat_id,
                            "skipping empty task-completion message in chat history (event-only row with no body)",
                        );
                        model_history.row_ends.push(result.len());
                        continue;
                    }
                    let content = task_completion_content(msg);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::LiveCall => {
                    let content = format!("[LIVE_CALL] {}", msg.content);
                    result.push(
                        build_user_message(
                            &content,
                            &msg.attachments,
                            &self.user_service,
                            &self.storage_service,
                        )
                        .await,
                    );
                }
                MessageRole::Agent => {
                    let other_handle =
                        resolve_other_agent_handle(msg, &ctx.agent_id, &self.agent_service).await;
                    if let Some(tes) = te_map.get(&msg.id) {
                        convert_agent_with_tool_calls(
                            msg,
                            tes,
                            &ctx.agent_id,
                            other_handle.as_deref(),
                            result,
                        );
                    } else if let Some(m) =
                        convert_agent_message(msg, &ctx.agent_id, other_handle.as_deref())
                    {
                        result.push(m);
                    }
                }
                MessageRole::System => {
                    if !msg.content.is_empty() {
                        result.push(RigMessage::user(&msg.content));
                    }
                }
            };
            model_history.row_ends.push(result.len());
        }
        attach_legacy_origins(&mut model_history, &ctx.model_config);
        model_history
    }

    fn system_prompt(&self) -> Option<String> {
        self.inbound_prompt.clone()
    }
}

fn group_tool_calls_by_message(tool_calls: &[ToolCall]) -> HashMap<String, Vec<&ToolCall>> {
    let mut map: HashMap<String, Vec<&ToolCall>> = HashMap::new();
    for te in tool_calls {
        map.entry(te.message_id.clone()).or_default().push(te);
    }
    map
}

/// Validate provider metadata before the infallible projection builder runs.
pub fn validate_provider_turns(
    messages: &[Message],
    tool_calls: &[ToolCall],
) -> Result<(), crate::core::error::AppError> {
    for message in messages {
        {
            serde_json::from_value::<std::collections::BTreeMap<u32, AssistantMessage>>(
                serde_json::json!(message.provider_turns),
            )
            .map_err(|error| {
                crate::core::error::AppError::Validation(format!(
                    "Invalid provider turns on message {}: {error}",
                    message.id
                ))
            })?;
        }
        if let Some(raw) = message.reasoning.as_ref().and_then(|r| r.raw.as_ref()) {
            frona_harness::inference::history_compat::reasoning_blocks(raw.clone()).map_err(
                |error| {
                    crate::core::error::AppError::Validation(format!(
                        "Invalid reasoning on message {}: {error}",
                        message.id
                    ))
                },
            )?;
        }
    }
    for call in tool_calls
        .iter()
        .filter(|call| messages.iter().any(|message| message.id == call.message_id))
    {
        if call.name.is_empty() || call.provider_call_id.is_empty() {
            return Err(crate::core::error::AppError::Validation(format!(
                "Invalid stored tool identity on {}",
                call.id
            )));
        }
        if let Some(raw) = call.turn_reasoning.as_ref().and_then(|r| r.raw.as_ref()) {
            frona_harness::inference::history_compat::reasoning_blocks(raw.clone()).map_err(
                |error| {
                    crate::core::error::AppError::Validation(format!(
                        "Invalid reasoning on tool call {}: {error}",
                        call.id
                    ))
                },
            )?;
        }
    }
    Ok(())
}

fn saved_turn(msg: &Message, turn: Option<u32>) -> Option<AssistantMessage> {
    let value = match turn {
        Some(turn) => msg.provider_turns.get(&turn.to_string()),
        None => msg
            .provider_turns
            .iter()
            .max_by_key(|(turn, _)| turn.parse::<u32>().expect("provider turn index validated"))
            .map(|(_, value)| value),
    }?;
    Some(
        serde_json::from_value(value.clone())
            .expect("provider turns validated before conversation projection"),
    )
}

fn reasoning_for_replay(r: &crate::chat::message::models::Reasoning) -> Vec<AssistantContent> {
    let value = r.raw.clone().unwrap_or_else(|| serde_json::json!({
        "id":r.id, "content":[{"type":"text", "content":{"text":r.content, "signature":r.signature}}]
    }));
    frona_harness::inference::history_compat::reasoning_blocks(value)
        .expect("stored reasoning validated before conversation projection")
}

fn attach_legacy_origins(history: &mut ModelConversationHistory, model: &ModelConfig) {
    for message in &mut history.history {
        if let RigMessage::Assistant(assistant) = message
            && assistant.origin.is_none()
        {
            frona_harness::inference::history_compat::contextualize_legacy(assistant, model);
        }
    }
}

fn convert_agent_with_tool_calls(
    msg: &Message,
    tool_calls: &[&ToolCall],
    agent_id: &str,
    other_agent_handle: Option<&str>,
    result: &mut Vec<RigMessage>,
) {
    let is_self = msg.agent_id.as_deref() == Some(agent_id);
    if !is_self {
        if !msg.content.is_empty() {
            result.push(RigMessage::user(attribute_cross_agent(
                &msg.content,
                other_agent_handle,
            )));
        }
        return;
    }

    let mut turns: std::collections::BTreeMap<u32, Vec<&ToolCall>> =
        std::collections::BTreeMap::new();
    for te in tool_calls {
        turns.entry(te.turn).or_default().push(te);
    }

    for (turn, tes) in &turns {
        let mut assistant_items: Vec<AssistantContent> = Vec::new();
        // Per-turn reasoning - stamped on the first tool_call of the turn at
        // begin time. Replayed here so thinking-mode providers (DeepSeek,
        // Anthropic extended thinking) see the `reasoning_content` they
        // originally emitted; without it they reject the request with
        // `invalid_request_error` on resume after a HITL pause.
        if let Some(r) = tes.iter().find_map(|te| te.turn_reasoning.as_ref()) {
            assistant_items.extend(reasoning_for_replay(r));
        }
        if let Some(text) = tes.iter().find_map(|te| te.turn_text.as_deref())
            && !text.is_empty()
        {
            assistant_items.push(AssistantContent::text(text));
        }
        for te in tes {
            let call = frona_harness::inference::history_compat::legacy_call(serde_json::json!({
                "id":te.provider_call_id,
                "provider":if te.provider_call_id.is_empty() { None } else { Some(serde_json::json!({"call_id":te.provider_call_id, "item_id":te.provider_item_id})) },
                "function":{"name":te.name,"arguments":te.arguments}, "signature":te.signature,
                "additional_params":null
            })).expect("persisted tool calls have valid identities and names");
            assistant_items.push(AssistantContent::ToolCall(call));
        }
        let mut assistant =
            saved_turn(msg, Some(*turn)).unwrap_or_else(|| AssistantMessage::new(assistant_items));
        // A stopped loop may have recorded a proposal it never started. Only
        // calls with persisted executions belong to this completed history view.
        let mut remaining = tes.clone();
        assistant.content.retain(|item| {
            let AssistantContent::ToolCall(call) = item else {
                return true;
            };
            if let Some(index) = remaining.iter().position(|te| {
                te.provider_call_id == call.id.wire() && te.name == call.function.name.as_str()
            }) {
                remaining.remove(index);
                true
            } else {
                false
            }
        });
        crate::tool::persistent_dispatch::project_assistant(&mut assistant);
        let calls: Vec<_> = assistant.tool_calls().cloned().collect();
        if !assistant.content.is_empty() {
            result.push(RigMessage::Assistant(assistant));
        }

        let tool_results: Vec<UserContent> = tes
            .iter()
            .map(|te| {
                let id = calls
                    .iter()
                    .find(|call| {
                        call.id.wire() == te.provider_call_id
                            && call.function.name.as_str() == te.name
                    })
                    .map(|call| call.id.clone())
                    .unwrap_or_else(|| CallId::from_wire(&te.provider_call_id));
                UserContent::ToolResult(ToolResult {
                    call: id,
                    name: te
                        .name
                        .clone()
                        .try_into()
                        .expect("stored tool name is nonempty"),
                    content: vec![ToolResultContent::text(&te.result)],
                    is_error: !te.success,
                })
            })
            .collect();
        if !tool_results.is_empty() {
            result.push(RigMessage::User {
                content: tool_results,
            });
        }
        for te in tes.iter() {
            if let Some(instructions) = &te.system_prompt {
                result.push(RigMessage::user(instructions));
            }
        }
    }

    let is_completed = msg.status.as_ref() == Some(&MessageStatus::Completed);
    if is_completed && !msg.content.is_empty() {
        let mut items: Vec<AssistantContent> = Vec::new();
        if let Some(r) = &msg.reasoning {
            items.extend(reasoning_for_replay(r));
        }
        items.push(AssistantContent::text(&msg.content));
        result.push(RigMessage::Assistant(
            saved_turn(msg, None)
                .filter(|m| m.tool_calls().next().is_none())
                .unwrap_or_else(|| AssistantMessage::new(items)),
        ));
    }
}

pub fn format_files_block_simple(content: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return content.to_string();
    }
    let paths: Vec<&str> = attachments.iter().map(|a| a.path.as_str()).collect();
    format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
}

pub fn is_embeddable_image(attachment: &Attachment) -> bool {
    is_image_content_type(&attachment.content_type) && !attachment.content_type.contains("svg")
}

pub fn convert_agent_message(
    msg: &Message,
    agent_id: &str,
    other_agent_handle: Option<&str>,
) -> Option<RigMessage> {
    if matches!(
        msg.status.as_ref(),
        Some(MessageStatus::Executing) | Some(MessageStatus::Paused)
    ) {
        return None;
    }
    let is_self = msg.agent_id.as_deref() == Some(agent_id);
    if is_self {
        if let Some(r) = &msg.reasoning {
            let mut items: Vec<AssistantContent> = reasoning_for_replay(r);
            if !msg.content.is_empty() {
                items.push(AssistantContent::text(&msg.content));
            }
            return Some(RigMessage::Assistant(
                saved_turn(msg, None).unwrap_or_else(|| AssistantMessage::new(items)),
            ));
        }
        // Empty Assistant text blocks make Anthropic reject the request.
        if msg.content.is_empty() {
            return None;
        }
        Some(
            saved_turn(msg, None)
                .map(RigMessage::Assistant)
                .unwrap_or_else(|| RigMessage::assistant(&msg.content)),
        )
    } else {
        if msg.content.is_empty() {
            return None;
        }
        Some(RigMessage::user(attribute_cross_agent(
            &msg.content,
            other_agent_handle,
        )))
    }
}

async fn resolve_other_agent_handle(
    msg: &Message,
    current_agent_id: &str,
    agent_service: &AgentService,
) -> Option<String> {
    let id = msg.agent_id.as_deref()?;
    if id == current_agent_id {
        return None;
    }
    agent_service
        .find_by_id(id)
        .await
        .ok()
        .flatten()
        .map(|a| a.handle.to_string())
}

/// Mirrors `task_completion_content`'s `<task_result>…</task_result>`
/// wrapper for the sibling task-delivery cross-agent flow.
fn attribute_cross_agent(content: &str, handle: Option<&str>) -> String {
    let label = handle.unwrap_or("unknown");
    format!("<system source=\"agent:{label}\">\n{content}\n</system>")
}

/// Wraps schema-bearing task results in `<task_result>{json}</task_result>`
/// so the parent LLM sees a consistent shape. Other cases pass through.
fn task_completion_content(msg: &Message) -> String {
    let has_schema = matches!(
        &msg.event,
        Some(MessageEvent::TaskCompletion {
            schema: Some(_),
            ..
        })
    );
    if has_schema && !msg.content.is_empty() {
        format!("<task_result>{}</task_result>", msg.content)
    } else {
        msg.content.clone()
    }
}

pub async fn resolve_attachment_path(
    attachment: &Attachment,
    user_service: &UserService,
    storage_service: &StorageService,
) -> String {
    let vpath = if let Some(user_id) = attachment.owner.strip_prefix("user:") {
        match user_service.find_by_id(user_id).await {
            Ok(Some(user)) => VirtualPath::user(&user.handle, &attachment.path),
            _ => return attachment.path.clone(),
        }
    } else if let Some(agent_id) = attachment.owner.strip_prefix("agent:") {
        VirtualPath::agent(agent_id, &attachment.path)
    } else {
        return attachment.path.clone();
    };

    storage_service
        .resolve_virtual_path(&vpath)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| attachment.path.clone())
}

pub async fn format_files_block(
    content: &str,
    attachments: &[Attachment],
    user_service: &UserService,
    storage_service: &StorageService,
) -> String {
    if attachments.is_empty() {
        return content.to_string();
    }
    let mut paths = Vec::with_capacity(attachments.len());
    for att in attachments {
        paths.push(resolve_attachment_path(att, user_service, storage_service).await);
    }
    format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
}

pub async fn build_user_message(
    content: &str,
    attachments: &[Attachment],
    user_service: &UserService,
    storage_service: &StorageService,
) -> RigMessage {
    if attachments.is_empty() {
        return RigMessage::user(content);
    }

    let mut images: Vec<(String, Attachment)> = Vec::new();
    let mut non_images: Vec<&Attachment> = Vec::new();

    for att in attachments {
        if is_embeddable_image(att) {
            let path = resolve_attachment_path(att, user_service, storage_service).await;
            images.push((path, att.clone()));
        } else {
            non_images.push(att);
        }
    }

    let text = if non_images.is_empty() {
        content.to_string()
    } else {
        let mut paths = Vec::with_capacity(non_images.len());
        for att in &non_images {
            paths.push(resolve_attachment_path(att, user_service, storage_service).await);
        }
        format!("{content}\n<files>\n{}\n</files>", paths.join("\n"))
    };

    if images.is_empty() {
        return RigMessage::user(&text);
    }

    let mut contents: Vec<UserContent> = vec![UserContent::text(&text)];

    for (resolved_path, att) in &images {
        if let Ok(bytes) = tokio::fs::read(resolved_path).await {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            contents.push(UserContent::Image(rig_core::completion::message::Image {
                data: DocumentSourceKind::Base64(b64),
                media_type: ImageMediaType::from_mime_type(&att.content_type),
                detail: None,
                native: None,
            }));
        }
    }

    if contents.len() == 1 {
        return RigMessage::user(&text);
    }

    RigMessage::User { content: contents }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::task::models::TaskStatus;
    use crate::chat::message::models::{MessageEvent, MessageRole};
    use chrono::Utc;

    fn make_message(role: MessageRole, content: &str) -> Message {
        Message {
            agent_context_signature: None,
            processed_content: None,
            provider_turns: Default::default(),
            id: "m1".to_string(),
            chat_id: "c1".to_string(),
            role,
            content: content.to_string(),
            agent_id: None,
            event: None,
            attachments: vec![],
            contact_id: None,
            status: None,
            error: None,
            reasoning: None,
            from_address: None,
            delivery: None,
            dispatch_mode: None,
            command: None,
            metadata: Default::default(),
            created_at: Utc::now(),
        }
    }

    fn make_agent_message(content: &str, agent_id: &str) -> Message {
        Message {
            agent_id: Some(agent_id.to_string()),
            ..make_message(MessageRole::Agent, content)
        }
    }

    #[test]
    fn agent_same_id_converts_to_assistant() {
        let msg = make_agent_message("hello", "agent-1");
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), RigMessage::Assistant(_)));
    }

    #[test]
    fn agent_different_id_converts_to_user() {
        let msg = make_agent_message("task instruction", "agent-2");
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), RigMessage::User { .. }));
    }

    #[test]
    fn agent_self_with_empty_content_is_skipped() {
        let mut msg = make_agent_message("", "agent-1");
        msg.status = Some(MessageStatus::Failed);
        assert!(convert_agent_message(&msg, "agent-1", None).is_none());
    }

    #[test]
    fn agent_self_with_empty_content_completed_is_skipped() {
        let mut msg = make_agent_message("", "agent-1");
        msg.status = Some(MessageStatus::Completed);
        assert!(convert_agent_message(&msg, "agent-1", None).is_none());
    }

    #[test]
    fn agent_other_with_empty_content_is_skipped() {
        let mut msg = make_agent_message("", "agent-2");
        msg.status = Some(MessageStatus::Failed);
        assert!(convert_agent_message(&msg, "agent-1", None).is_none());
    }

    #[test]
    fn agent_self_with_reasoning_and_empty_content_still_emits_reasoning() {
        let mut msg = make_agent_message("", "agent-1");
        msg.reasoning = Some(crate::chat::message::models::Reasoning {
            id: None,
            content: "thinking".into(),
            signature: None,
            raw: None,
        });
        assert!(convert_agent_message(&msg, "agent-1", None).is_some());
    }

    #[test]
    fn format_files_block_simple_no_attachments() {
        assert_eq!(format_files_block_simple("hello world", &[]), "hello world");
        assert!(!format_files_block_simple("hello", &[]).contains("<files>"));
    }

    #[test]
    fn format_files_block_simple_with_attachments() {
        let attachments = vec![Attachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size_bytes: 1024,
            owner: "user:uid".to_string(),
            path: "report.pdf".to_string(),
            url: None,
        }];
        let result = format_files_block_simple("check this file", &attachments);
        assert!(result.contains("check this file"));
        assert!(result.contains("<files>"));
        assert!(result.contains("report.pdf"));
        assert!(result.contains("</files>"));
    }

    #[test]
    fn is_embeddable_image_png() {
        let att = Attachment {
            filename: "logo.png".into(),
            content_type: "image/png".into(),
            size_bytes: 100,
            owner: "user:uid".into(),
            path: "logo.png".into(),
            url: None,
        };
        assert!(is_embeddable_image(&att));
    }

    #[test]
    fn is_embeddable_image_svg_excluded() {
        let att = Attachment {
            filename: "icon.svg".into(),
            content_type: "image/svg+xml".into(),
            size_bytes: 100,
            owner: "user:uid".into(),
            path: "icon.svg".into(),
            url: None,
        };
        assert!(!is_embeddable_image(&att));
    }

    #[test]
    fn is_embeddable_image_pdf_excluded() {
        let att = Attachment {
            filename: "doc.pdf".into(),
            content_type: "application/pdf".into(),
            size_bytes: 100,
            owner: "user:uid".into(),
            path: "doc.pdf".into(),
            url: None,
        };
        assert!(!is_embeddable_image(&att));
    }

    #[test]
    fn task_completion_converts_to_user_message_via_simple_format() {
        let msg = Message {
            event: Some(MessageEvent::TaskCompletion {
                task_id: "t1".to_string(),
                chat_id: Some("c2".to_string()),
                status: TaskStatus::Completed,
                summary: None,
                schema: None,
            }),
            ..make_message(MessageRole::TaskCompletion, "Task 'research' completed.")
        };
        assert_eq!(msg.role, MessageRole::TaskCompletion);
        let text = format_files_block_simple(&msg.content, &msg.attachments);
        assert_eq!(text, "Task 'research' completed.");
    }

    #[test]
    fn task_completion_with_attachments() {
        let msg = Message {
            event: Some(MessageEvent::TaskCompletion {
                task_id: "t1".to_string(),
                chat_id: None,
                status: TaskStatus::Completed,
                summary: None,
                schema: None,
            }),
            attachments: vec![Attachment {
                filename: "output.csv".to_string(),
                content_type: "text/csv".to_string(),
                size_bytes: 512,
                owner: "agent:dev".to_string(),
                path: "output.csv".to_string(),
                url: None,
            }],
            ..make_message(MessageRole::TaskCompletion, "Task done")
        };
        let text = format_files_block_simple(&msg.content, &msg.attachments);
        assert!(text.contains("Task done"));
        assert!(text.contains("output.csv"));
    }

    #[test]
    fn agent_with_reasoning_includes_reasoning_in_assistant_message() {
        use crate::chat::message::models::Reasoning;

        let msg = Message {
            agent_id: Some("agent-1".to_string()),
            reasoning: Some(Reasoning {
                id: Some("reasoning-1".to_string()),
                content: "Let me think about this...".to_string(),
                signature: Some("sig-abc".to_string()),
                raw: None,
            }),
            ..make_message(MessageRole::Agent, "Here is my answer")
        };
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        let rig_msg = result.unwrap();
        if let RigMessage::Assistant(assistant) = &rig_msg {
            let content = &assistant.content;
            let has_reasoning = content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_)));
            assert!(
                has_reasoning,
                "Expected reasoning content in assistant message"
            );
            let has_text = content
                .iter()
                .any(|c| matches!(c, AssistantContent::Text(_)));
            assert!(has_text, "Expected text content in assistant message");
        } else {
            panic!("Expected Assistant message");
        }
    }

    #[test]
    fn executing_agent_message_is_skipped() {
        let msg = Message {
            agent_id: Some("agent-1".to_string()),
            status: Some(MessageStatus::Executing),
            ..make_message(MessageRole::Agent, "")
        };
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_none(), "Executing messages should be skipped");
    }

    #[test]
    fn agent_without_reasoning_is_unchanged() {
        let msg = make_agent_message("hello", "agent-1");
        let result = convert_agent_message(&msg, "agent-1", None);
        assert!(result.is_some());
        if let RigMessage::Assistant(assistant) = result.unwrap() {
            let content = assistant.content;
            let has_reasoning = content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_)));
            assert!(!has_reasoning);
        }
    }
}
