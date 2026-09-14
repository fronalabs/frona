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

use crate::agent::task::models::{Task, TaskKind, TaskStatus};
use crate::chat::broadcast::BroadcastService;
use crate::chat::message::models::MessageStatus;
use crate::core::repository::Repository;
use crate::db::repo::generic::SurrealRepo;
use crate::inference::tool_call::ToolCall;
use chrono::TimeZone;
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;

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
    let one_message_tokens = crate::inference::context::estimate_tokens("Agent: ")
        + crate::inference::context::estimate_tokens(&first.content);
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
    let first_text = crate::memory::pkm::consolidation::transcript::message_text(
        &first.id,
        &first.content,
        std::slice::from_ref(&call),
    );
    let first_tokens = crate::inference::context::estimate_tokens("Agent: ")
        + crate::inference::context::estimate_tokens(&first_text);

    let windows =
        consolidation_windows(vec![first, second], at(150), first_tokens, 100, &[call]).unwrap();

    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].0[0].id, "message-research");
    assert_eq!(windows[1].0[0].id, "message-next");
}

#[test]
fn completed_tasks_attach_to_the_next_agent_result_message() {
    let mut first = msg(at(100), Some(MessageStatus::Completed));
    first.id = "task-completion-1".into();
    first.role = MessageRole::TaskCompletion;
    first.event = Some(MessageEvent::TaskCompletion {
        task_id: "task-1".into(),
        chat_id: Some("task-chat-1".into()),
        status: crate::agent::task::models::TaskStatus::Completed,
        summary: None,
        schema: None,
    });
    let mut second = msg(at(110), Some(MessageStatus::Completed));
    second.id = "task-completion-2".into();
    second.role = MessageRole::TaskCompletion;
    second.event = Some(MessageEvent::TaskCompletion {
        task_id: "task-2".into(),
        chat_id: Some("task-chat-2".into()),
        status: crate::agent::task::models::TaskStatus::Completed,
        summary: None,
        schema: None,
    });
    let mut result = msg(at(120), Some(MessageStatus::Completed));
    result.id = "parent-result".into();
    let mut later = msg(at(130), Some(MessageStatus::Completed));
    later.id = "later-agent".into();

    let links = completed_task_result_links(&[first, second, result, later]);

    assert_eq!(
        links.get("parent-result"),
        Some(&vec!["task-1".into(), "task-2".into()])
    );
    assert!(!links.contains_key("later-agent"));
}

fn stored_task(id: &str, chat_id: &str, source_chat_id: Option<&str>) -> Task {
    Task {
        id: id.into(),
        user_id: "user".into(),
        agent_id: "agent".into(),
        space_id: None,
        chat_id: Some(chat_id.into()),
        title: id.into(),
        description: String::new(),
        status: TaskStatus::Completed,
        kind: TaskKind::Direct {
            source_chat_id: source_chat_id.map(str::to_string),
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        quarantined: false,
        result_schema: None,
        result_description: None,
        created_at: at(1),
        updated_at: at(1),
    }
}

#[test]
fn task_lifecycle_text_contains_event_and_target_times() {
    let text = render_task_lifecycle("scheduled", "Review report", at(100), Some(at(200)));

    assert_eq!(
        text,
        "[task scheduled event_at=1970-01-01T00:01:40.000Z target_at=1970-01-01T00:03:20.000Z] Review report"
    );
}

#[test]
fn task_target_uses_run_time_and_recurring_fire_time() {
    let mut deferred = stored_task("deferred", "chat", Some("source"));
    deferred.run_at = Some(at(200));
    assert_eq!(task_target_at(&deferred), Some(at(200)));

    let mut recurring_run = stored_task("run", "chat", Some("source"));
    recurring_run.kind = TaskKind::CronRun {
        source_cron_id: "template".into(),
        source_chat_id: Some("source".into()),
        source_agent_id: Some("agent".into()),
        fire_at: at(300),
        sequence_num: 1,
    };
    assert_eq!(task_target_at(&recurring_run), Some(at(300)));
}

fn stored_call(id: &str, chat_id: &str, turn: u32) -> ToolCall {
    ToolCall {
        id: id.into(),
        chat_id: chat_id.into(),
        message_id: format!("message-{id}"),
        turn,
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
        turn_reasoning: None,
        created_at: at(turn as i64),
    }
}

#[tokio::test]
async fn task_tree_collection_reads_direct_and_nested_task_chats() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    let task_repo = SurrealRepo::<Task>::new(db.clone());
    let tool_repo = SurrealRepo::<ToolCall>::new(db);
    task_repo
        .create(&stored_task("root", "root-chat", None))
        .await
        .unwrap();
    task_repo
        .create(&stored_task("child", "child-chat", Some("root-chat")))
        .await
        .unwrap();
    task_repo
        .create(&stored_task(
            "grandchild",
            "grandchild-chat",
            Some("child-chat"),
        ))
        .await
        .unwrap();
    task_repo
        .create(&stored_task("other", "other-chat", None))
        .await
        .unwrap();
    tool_repo
        .create(&stored_call("root-call", "root-chat", 1))
        .await
        .unwrap();
    tool_repo
        .create(&stored_call("child-call", "child-chat", 2))
        .await
        .unwrap();
    tool_repo
        .create(&stored_call("grandchild-call", "grandchild-chat", 3))
        .await
        .unwrap();
    tool_repo
        .create(&stored_call("other-call", "other-chat", 4))
        .await
        .unwrap();
    let service = crate::agent::task::service::TaskService::new(task_repo, BroadcastService::new());

    let calls = collect_task_tree_tool_calls(&["root".into()], &service, &tool_repo)
        .await
        .unwrap();

    assert_eq!(
        calls
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["root-call", "child-call", "grandchild-call",]
    );
}
