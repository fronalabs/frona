use super::*;
use chrono::{TimeZone, Utc};
use serde_json::json;

use crate::evidence::ToolExecution;

fn call(
    id: &str,
    message_id: &str,
    turn: u32,
    name: &str,
    arguments: serde_json::Value,
    result: &str,
    success: bool,
) -> ToolExecution {
    ToolExecution {
        id: id.into(),
        chat_id: "chat-1".into(),
        message_id: message_id.into(),
        turn,

        name: name.into(),
        arguments,
        result: result.into(),
        success,

        user_response: false,
        created_at: Utc.timestamp_opt(i64::from(turn), 0).unwrap(),
    }
}

#[test]
fn projection_keeps_only_successful_recall_calls_in_message_order() {
    let calls = vec![
        call(
            "search-2",
            "agent-1",
            2,
            "memory_search",
            json!({"query":"phone"}),
            "Casey Owner — phone 555-0100",
            true,
        ),
        call(
            "shell",
            "agent-1",
            1,
            "shell",
            json!({"command":"pwd"}),
            "/tmp",
            true,
        ),
        call(
            "failed",
            "agent-1",
            3,
            "memory_search",
            json!({"query":"boss"}),
            "failed",
            false,
        ),
        call(
            "search-1",
            "agent-1",
            1,
            "memory_search",
            json!({"query":"Casey Owner"}),
            "Casey Owner — spouse",
            true,
        ),
    ];

    let projection = RecallProjection::new(&calls, |_| false);
    let projected = projection.for_message("agent-1");
    assert_eq!(
        projected
            .iter()
            .map(|call| call.local_id.as_str())
            .collect::<Vec<_>>(),
        vec!["T1", "T2"]
    );
    assert_eq!(
        projected
            .iter()
            .map(|call| call.arguments["query"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Casey Owner", "phone"]
    );
}

#[test]
fn projection_accepts_only_reads_inside_the_knowledge_vault() {
    let calls = vec![
        call(
            "page",
            "agent-1",
            1,
            "read",
            json!({"path":"/vault/Memory/people/casey-owner.md"}),
            "# Casey Owner",
            true,
        ),
        call(
            "secret",
            "agent-1",
            2,
            "read",
            json!({"path":"/vault/secrets.txt"}),
            "token",
            true,
        ),
    ];

    let projection = RecallProjection::new(&calls, |path| path.starts_with("/vault/Memory/"));
    let projected = projection.for_message("agent-1");
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].local_id, "T1");
    assert!(
        !projection
            .render_for_message("agent-1", "m2")
            .contains("token")
    );
}

#[test]
fn stored_result_lookup_is_scoped_and_bounded() {
    let calls = vec![call(
        "page",
        "agent-1",
        1,
        "read",
        json!({"path":"/vault/Memory/people/casey-owner.md"}),
        &"line\n".repeat(5_000),
        true,
    )];
    let projection = RecallProjection::new(&calls, |_| true);

    assert!(
        frona_harness::inference::context::estimate_tokens(&projection.read_result("T1").unwrap())
            <= RECALL_RESULT_TOKEN_CAP
    );
    assert!(projection.read_result("T999").is_none());
}
