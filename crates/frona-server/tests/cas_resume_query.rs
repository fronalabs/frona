//! Prototype + regression test for the combined barrier+CAS query that
//! `ChatService::mark_message_executing` uses to atomically:
//!   1. Verify the message is currently Paused.
//!   2. Verify no tool_call attached to it has a pending HITL.
//!   3. Flip status to Executing.
//!   4. Return whether the flip happened (dedup signal for concurrent
//!      `resolve_hitl` workers).
//!
//! These run against an in-memory SurrealDB so they exercise the real
//! SurrealQL parser/optimizer the way prod will.

use chrono::Utc;
use frona::chat::message::models::{Message, MessageRole, MessageStatus};
use frona::db::init as db;
use frona::inference::hitl::{Hitl, HitlRequest};
use frona::inference::tool_call::{ToolCall, ToolStatus};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn make_message(id: &str, status: MessageStatus) -> Message {
    Message {
        id: id.to_string(),
        chat_id: "chat-1".into(),
        role: MessageRole::Agent,
        content: String::new(),
        agent_id: Some("agent-1".into()),
        event: None,
        attachments: vec![],
        contact_id: None,
        status: Some(status),
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

fn make_tool_call(id: &str, msg_id: &str, hitl_status: Option<ToolStatus>) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        chat_id: "chat-1".into(),
        message_id: msg_id.to_string(),
        turn: 0,
        provider_call_id: format!("pc-{id}"),
        name: "ask_user_question".into(),
        arguments: serde_json::json!({"question": "?"}),
        result: String::new(),
        success: false,
        duration_ms: 0,
        hitl: hitl_status.map(|status| Hitl {
            prompt: "?".into(),
            url: "/chats/c".into(),
            request: HitlRequest::Question { options: vec![] },
            status,
            response: None,
            delivery: None,
        }),
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        turn_reasoning: None,
        created_at: Utc::now(),
    }
}

/// The SurrealQL we want to run. Returns the message id if (and only if) the
/// flip happened. An empty result means: not currently Paused, OR there is a
/// pending HITL, OR another concurrent caller already flipped it.
const CAS_QUERY: &str = "UPDATE message
    SET status = $new_status
    WHERE meta::id(id) = $msg_id
      AND status = $old_status
      AND array::len(
            (SELECT VALUE id FROM tool_call
             WHERE message_id = $msg_id
               AND hitl.status = $pending)
          ) = 0
    RETURN meta::id(id) AS id";

async fn create(db: &Surreal<Db>, msg: &Message) {
    // Use raw CREATE to avoid the SDK trying to deserialize the returned
    // record as our Message struct (which expects id: String, but the row's
    // id field is a SurrealDB record id).
    db.query("CREATE type::record('message', $id) CONTENT $body")
        .bind(("id", msg.id.clone()))
        .bind(("body", msg.clone()))
        .await
        .unwrap();
}

async fn create_tc(db: &Surreal<Db>, tc: &ToolCall) {
    db.query("CREATE type::record('tool_call', $id) CONTENT $body")
        .bind(("id", tc.id.clone()))
        .bind(("body", tc.clone()))
        .await
        .unwrap();
}

async fn run_cas(db: &Surreal<Db>, msg_id: &str) -> Vec<serde_json::Value> {
    let mut r = db
        .query(CAS_QUERY)
        .bind(("msg_id", msg_id.to_string()))
        .bind(("new_status", MessageStatus::Executing))
        .bind(("old_status", MessageStatus::Paused))
        .bind(("pending", ToolStatus::Pending))
        .await
        .unwrap();
    r.take(0).unwrap()
}

async fn reload_status(db: &Surreal<Db>, msg_id: &str) -> Option<MessageStatus> {
    let mut r = db
        .query("SELECT *, meta::id(id) AS id FROM message WHERE meta::id(id) = $msg_id")
        .bind(("msg_id", msg_id.to_string()))
        .await
        .unwrap();
    let rows: Vec<Message> = r.take(0).unwrap();
    rows.into_iter().next().and_then(|m| m.status)
}

#[tokio::test]
async fn cas_flips_paused_to_executing_when_no_pending_hitls() {
    let db = test_db().await;
    let msg = make_message("m1", MessageStatus::Paused);
    create(&db, &msg).await;
    create_tc(
        &db,
        &make_tool_call("tc1", "m1", Some(ToolStatus::Resolved)),
    )
    .await;

    let result = run_cas(&db, "m1").await;
    assert_eq!(result.len(), 1, "CAS should flip the row");
    assert_eq!(
        reload_status(&db, "m1").await,
        Some(MessageStatus::Executing)
    );
}

#[tokio::test]
async fn cas_no_op_when_pending_hitl_present() {
    let db = test_db().await;
    create(&db, &make_message("m1", MessageStatus::Paused)).await;
    create_tc(
        &db,
        &make_tool_call("tc1", "m1", Some(ToolStatus::Resolved)),
    )
    .await;
    create_tc(&db, &make_tool_call("tc2", "m1", Some(ToolStatus::Pending))).await;

    let result = run_cas(&db, "m1").await;
    assert!(
        result.is_empty(),
        "CAS must not flip while a HITL is pending"
    );
    assert_eq!(
        reload_status(&db, "m1").await,
        Some(MessageStatus::Paused),
        "status must stay Paused"
    );
}

#[tokio::test]
async fn cas_no_op_when_status_already_executing() {
    let db = test_db().await;
    create(&db, &make_message("m1", MessageStatus::Executing)).await;
    create_tc(
        &db,
        &make_tool_call("tc1", "m1", Some(ToolStatus::Resolved)),
    )
    .await;

    let result = run_cas(&db, "m1").await;
    assert!(
        result.is_empty(),
        "second concurrent worker sees row already flipped"
    );
}

#[tokio::test]
async fn cas_no_op_when_status_completed() {
    let db = test_db().await;
    create(&db, &make_message("m1", MessageStatus::Completed)).await;

    let result = run_cas(&db, "m1").await;
    assert!(result.is_empty());
    assert_eq!(
        reload_status(&db, "m1").await,
        Some(MessageStatus::Completed)
    );
}

#[tokio::test]
async fn cas_flips_when_message_has_no_tool_calls_at_all() {
    let db = test_db().await;
    create(&db, &make_message("m1", MessageStatus::Paused)).await;
    // No tool_calls — empty subquery returns array::len 0.

    let result = run_cas(&db, "m1").await;
    assert_eq!(result.len(), 1);
    assert_eq!(
        reload_status(&db, "m1").await,
        Some(MessageStatus::Executing)
    );
}

#[tokio::test]
async fn cas_ignores_pending_hitls_on_other_messages() {
    let db = test_db().await;
    create(&db, &make_message("m1", MessageStatus::Paused)).await;
    create(&db, &make_message("m2", MessageStatus::Paused)).await;
    // Pending HITL belongs to a *different* message.
    create_tc(&db, &make_tool_call("tc1", "m2", Some(ToolStatus::Pending))).await;

    let result = run_cas(&db, "m1").await;
    assert_eq!(result.len(), 1, "m1 has no pending HITLs of its own");
    assert_eq!(
        reload_status(&db, "m1").await,
        Some(MessageStatus::Executing)
    );
    assert_eq!(
        reload_status(&db, "m2").await,
        Some(MessageStatus::Paused),
        "m2 must not be touched"
    );
}

#[tokio::test]
async fn cas_dedup_only_one_concurrent_caller_wins() {
    let db = test_db().await;
    create(&db, &make_message("m1", MessageStatus::Paused)).await;
    create_tc(
        &db,
        &make_tool_call("tc1", "m1", Some(ToolStatus::Resolved)),
    )
    .await;

    // Race two CAS calls. SurrealDB's UPDATE-WHERE is the dedup gate: exactly
    // one should report having flipped the row.
    let db_a = db.clone();
    let db_b = db.clone();
    let (a, b) = tokio::join!(async move { run_cas(&db_a, "m1").await }, async move {
        run_cas(&db_b, "m1").await
    });

    let total = a.len() + b.len();
    assert_eq!(
        total,
        1,
        "exactly one of the racing workers must report a flip; got A={} B={}",
        a.len(),
        b.len()
    );
    assert_eq!(
        reload_status(&db, "m1").await,
        Some(MessageStatus::Executing)
    );
}
