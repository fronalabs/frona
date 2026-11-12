use super::*;

fn consolidation_window(
    messages: Vec<MessageResponse>,
    watermark: DateTime<Utc>,
    max_tokens: usize,
    max_messages: usize,
) -> Result<(Vec<MessageResponse>, Option<DateTime<Utc>>), AppError> {
    Ok(
        consolidation_windows(messages, watermark, max_tokens, max_messages, &[])?
            .into_iter()
            .next()
            .unwrap_or_default(),
    )
}

use crate::chat::message::models::{MessageResponse, MessageRole, MessageStatus};
use crate::inference::tool_call::ToolCall;
use chrono::TimeZone;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

fn msg(created: DateTime<Utc>, status: Option<MessageStatus>) -> MessageResponse {
    MessageResponse {
        id: String::new(),
        chat_id: String::new(),
        role: MessageRole::Agent,
        content: "x".into(),
        agent_id: None,
        event: None,
        attachments: Vec::new(),
        contact_id: None,
        status,
        error: None,
        reasoning: None,
        from_address: None,
        delivery: None,
        tool_calls: Vec::new(),
        command: None,
        metadata: Default::default(),
        created_at: created,
    }
}

#[test]
fn window_takes_terminal_messages_past_watermark() {
    let msgs = vec![
        msg(at(100), Some(MessageStatus::Completed)),
        msg(at(200), Some(MessageStatus::Completed)),
        msg(at(300), None), // legacy/no-status counts as terminal
    ];
    let (new, advance) = consolidation_window(msgs, at(150), 1_000, 100).unwrap();
    assert_eq!(new.len(), 2);
    assert_eq!(advance, Some(at(300)));
}

#[test]
fn window_stops_before_the_in_flight_boundary() {
    let msgs = vec![
        msg(at(200), Some(MessageStatus::Completed)),
        msg(at(250), Some(MessageStatus::Completed)),
        msg(at(300), Some(MessageStatus::Executing)),
    ];
    let (new, advance) = consolidation_window(msgs, at(150), 1_000, 100).unwrap();
    assert_eq!(new.len(), 2, "only the terminal prefix");
    assert_eq!(
        advance,
        Some(at(250)),
        "watermark holds below the in-flight message"
    );
}

#[test]
fn window_empty_when_only_in_flight_is_past_watermark() {
    let msgs = vec![
        msg(at(100), Some(MessageStatus::Completed)),
        msg(at(300), Some(MessageStatus::Executing)),
    ];
    let (new, advance) = consolidation_window(msgs, at(150), 1_000, 100).unwrap();
    assert!(new.is_empty());
    assert_eq!(
        advance, None,
        "watermark does not advance past the in-flight message"
    );
}

#[test]
fn window_paused_bounds_and_excludes_terminal_after_it() {
    let msgs = vec![
        msg(at(200), Some(MessageStatus::Completed)),
        msg(at(250), Some(MessageStatus::Paused)),
        msg(at(300), Some(MessageStatus::Completed)),
    ];
    let (new, advance) = consolidation_window(msgs, at(150), 1_000, 100).unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(advance, Some(at(200)));
}

#[test]
fn window_stops_at_message_limit_and_advances_to_last_selected() {
    let msgs = vec![msg(at(200), None), msg(at(300), None), msg(at(400), None)];
    let (new, advance) = consolidation_window(msgs, at(150), 1_000, 2).unwrap();
    assert_eq!(new.len(), 2);
    assert_eq!(advance, Some(at(300)));
}

#[test]
fn window_stops_before_token_limit_and_advances_to_last_selected() {
    let mut first = msg(at(200), None);
    first.content = "a".repeat(40);
    let mut second = msg(at(300), None);
    second.content = "b".repeat(40);
    let one_message_tokens = frona_harness::inference::context::estimate_tokens("Agent: ")
        + frona_harness::inference::context::estimate_tokens(&first.content);
    let (new, advance) =
        consolidation_window(vec![first, second], at(150), one_message_tokens, 100).unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(advance, Some(at(200)));
}

#[test]
fn window_processes_one_message_larger_than_token_limit_whole() {
    let mut oversized = msg(at(200), None);
    oversized.id = "too-large".into();
    oversized.content = "x".repeat(100);
    let (new, advance) = consolidation_window(vec![oversized], at(150), 1, 100).unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].id, "too-large");
    assert_eq!(new[0].content.len(), 100);
    assert_eq!(advance, Some(at(200)));
}

#[test]
fn windows_partition_the_complete_terminal_prefix_without_cutting_messages() {
    let msgs = vec![msg(at(200), None), msg(at(300), None), msg(at(400), None)];
    let windows = consolidation_windows(msgs, at(150), 1_000, 2, &[]).unwrap();
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].0.len(), 2);
    assert_eq!(windows[0].1, Some(at(300)));
    assert_eq!(windows[1].0.len(), 1);
    assert_eq!(windows[1].1, Some(at(400)));
}

#[test]
fn windows_count_tool_turn_text_toward_the_token_limit() {
    let mut first = msg(at(200), None);
    first.id = "message-research".into();
    first.content = "short final".into();
    let mut second = msg(at(300), None);
    second.id = "message-next".into();
    let mut call = stored_call("research", "chat", 0);
    call.message_id = first.id.clone();
    call.turn_text = Some("firmware procedure ".repeat(50));
    let first_text = crate::memory::pkm::ingest::transcript::message_text(
        &first.id,
        &first.content,
        std::slice::from_ref(&call),
    );
    let first_tokens = frona_harness::inference::context::estimate_tokens("Agent: ")
        + frona_harness::inference::context::estimate_tokens(&first_text);

    let windows =
        consolidation_windows(vec![first, second], at(150), first_tokens, 100, &[call]).unwrap();

    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].0[0].id, "message-research");
    assert_eq!(windows[1].0[0].id, "message-next");
}

fn stored_call(id: &str, chat_id: &str, turn: u32) -> ToolCall {
    ToolCall {
        id: id.into(),
        chat_id: chat_id.into(),
        message_id: format!("message-{id}"),
        turn,
        provider_item_id: None,
        provider_call_id: format!("provider-{id}"),
        name: "web_fetch".into(),
        arguments: serde_json::json!({"url":format!("https://example.test/{id}")}),
        result: format!("result {id}"),
        success: true,
        duration_ms: 1,
        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: at(turn as i64),
    }
}
