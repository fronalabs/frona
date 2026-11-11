use serde::{Deserialize, Serialize};

/// A caller-scoped source conversation; messages are supplied in chronological order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    pub messages: Vec<TranscriptMessage>,
    pub invocations: Vec<Invocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptMessage {
    pub id: String,
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Agent,
    System,
    Event,
}

/// Application-independent projection of a recorded invocation. Provider IDs,
/// hidden reasoning, credentials, and lifecycle controls are deliberately absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    pub id: String,
    pub message_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: String,
    pub turn: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub turn_text: Option<String>,
}

pub(super) fn message_text(
    message_id: &str,
    final_text: &str,
    tool_calls: &[Invocation],
) -> String {
    let mut turns = tool_calls
        .iter()
        .filter(|call| call.message_id == message_id)
        .filter_map(|call| call.turn_text.as_deref().map(|text| (call, text.trim())))
        .filter(|(_, text)| !text.is_empty())
        .collect::<Vec<_>>();
    turns.sort_by(|(left, _), (right, _)| {
        (left.turn, left.created_at, &left.id).cmp(&(right.turn, right.created_at, &right.id))
    });
    turns
        .into_iter()
        .map(|(_, text)| frona_text::redaction::redact_text(text))
        .chain((!final_text.trim().is_empty()).then(|| final_text.trim().to_string()))
        .collect::<Vec<_>>()
        .join("\n\n")
}
