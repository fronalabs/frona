use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::Entity;
use crate::inference::hitl::Hitl;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "lowercase")]
#[surreal(crate = "surrealdb::types", lowercase)]
pub enum ToolStatus {
    Pending,
    Resolved,
    Denied,
}

/// Terminal signals emitted by task-control tools (`complete_task`,
/// `fail_task`, `defer_task`). Distinct from `Hitl` (which pauses awaiting
/// human input); `TaskEvent` terminates the tool loop with a task outcome.
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[serde(tag = "type", content = "data")]
#[surreal(crate = "surrealdb::types", tag = "type", content = "data")]
pub enum TaskEvent {
    Completion {
        task_id: String,
        chat_id: Option<String>,
        status: crate::agent::task::models::TaskStatus,
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deliverables: Vec<crate::storage::Attachment>,
    },
    Deferred {
        task_id: String,
        delay_minutes: u32,
        reason: String,
    },
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "tool_call")]
pub struct ToolCall {
    pub id: String,
    pub chat_id: String,
    pub message_id: String,
    pub turn: u32,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: String,
    pub success: bool,
    pub duration_ms: u64,
    /// Pause marker for HITL prompts. `Some` when the tool emitted a Hitl;

    /// status starts as `Pending`, flips to `Resolved`/`Denied` on resolution.
    pub hitl: Option<Hitl>,
    /// Terminal signal from task-control tools. Mutually exclusive with `hitl`.
    pub task_event: Option<TaskEvent>,
    pub system_prompt: Option<String>,
    pub description: Option<String>,
    pub turn_text: Option<String>,
    /// Per-turn reasoning emitted by the model right before this tool call.
    /// Set only on the FIRST tool_call of each turn (paired with `turn_text`).
    /// Required for thinking-mode providers (DeepSeek, Anthropic extended
    /// thinking) which mandate `reasoning_content` be replayed in subsequent
    /// chat-completion requests — without it, resume after a HITL pause errors
    /// with `invalid_request_error`.
    pub turn_reasoning: Option<crate::chat::message::models::Reasoning>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResponse {
    pub id: String,
    pub chat_id: String,
    pub message_id: String,
    pub turn: u32,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: String,
    pub success: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl: Option<Hitl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_event: Option<TaskEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_reasoning: Option<crate::chat::message::models::Reasoning>,
    pub created_at: DateTime<Utc>,
}

impl From<ToolCall> for ToolCallResponse {
    fn from(te: ToolCall) -> Self {
        Self {
            id: te.id,
            chat_id: te.chat_id,
            message_id: te.message_id,
            turn: te.turn,
            provider_call_id: te.provider_call_id,
            name: te.name,
            arguments: te.arguments,
            result: te.result,
            success: te.success,
            duration_ms: te.duration_ms,
            hitl: te.hitl,
            task_event: te.task_event,
            system_prompt: te.system_prompt,
            description: te.description,
            turn_text: te.turn_text,
            turn_reasoning: te.turn_reasoning,
            created_at: te.created_at,
        }
    }
}
