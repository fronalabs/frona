use chrono::{DateTime, Utc};

use crate::chat::message::models::{MessageResponse, MessageRole, MessageStatus};
use crate::core::error::AppError;
use crate::inference::tool_call::ToolCall;

pub(super) type ConsolidationWindow = (Vec<MessageResponse>, Option<DateTime<Utc>>);

pub(super) fn consolidation_windows(
    messages: Vec<MessageResponse>,
    watermark: DateTime<Utc>,
    max_tokens: usize,
    max_messages: usize,
    tool_calls: &[ToolCall],
) -> Result<Vec<ConsolidationWindow>, AppError> {
    let in_flight = |m: &MessageResponse| {
        matches!(
            m.status,
            Some(MessageStatus::Executing | MessageStatus::Paused)
        )
    };
    let t_inflight = messages
        .iter()
        .filter(|m| in_flight(m))
        .map(|m| m.created_at)
        .min()
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    let eligible: Vec<MessageResponse> = messages
        .into_iter()
        .filter(|m| m.created_at > watermark && m.created_at < t_inflight && !in_flight(m))
        .collect();
    let max_tokens = max_tokens.max(1);
    let max_messages = max_messages.max(1);
    let mut windows = Vec::new();
    let mut current = Vec::new();
    let mut tokens = 0usize;
    for message in eligible {
        let speaker = match message.role {
            MessageRole::User => "User: ",
            MessageRole::Agent => "Agent: ",
            MessageRole::Contact => "Contact: ",
            _ => "",
        };
        let text = if message.role == MessageRole::Agent {
            crate::memory::pkm::consolidation::transcript::message_text(
                &message.id,
                &message.content,
                tool_calls,
            )
        } else {
            message.content.trim().to_string()
        };
        let message_tokens = if speaker.is_empty() || text.is_empty() {
            0
        } else {
            frona_harness::inference::context::estimate_tokens(speaker)
                + frona_harness::inference::context::estimate_tokens(&text)
        };
        if !current.is_empty()
            && (current.len() >= max_messages || tokens + message_tokens > max_tokens)
        {
            let advance_to = current
                .iter()
                .map(|item: &MessageResponse| item.created_at)
                .max();
            windows.push((std::mem::take(&mut current), advance_to));
            tokens = 0;
        }
        if current.is_empty() && message_tokens > max_tokens {
            tracing::warn!(
                message = %message.id,
                estimated_tokens = message_tokens,
                max_tokens,
                "single message exceeds extract token limit; processing it whole"
            );
        }
        tokens += message_tokens;
        current.push(message);
    }
    if !current.is_empty() {
        let advance_to = current
            .iter()
            .map(|item: &MessageResponse| item.created_at)
            .max();
        windows.push((current, advance_to));
    }
    Ok(windows)
}
