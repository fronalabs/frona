use chrono::Utc;
use frona::core::repository::Repository;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::tool_calls::ToolCallRepository;
use frona::inference::tool_call::ToolCall;
use frona::inference::tool_call::ToolStatus;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_tool_call(chat_id: &str, message_id: &str, turn: u32, name: &str) -> ToolCall {
    ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: chat_id.to_string(),
        message_id: message_id.to_string(),
        turn,
        provider_call_id: format!("call-{}", frona::core::repository::new_id()),
        name: name.to_string(),
        arguments: serde_json::json!({"query": "test"}),
        result: "tool result".to_string(),
        success: true,
        duration_ms: 42,

        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn create_and_find_by_id() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let te = test_tool_call("chat-1", "msg-1", 0, "search_web");
    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().expect("should find");
    assert_eq!(found.name, "search_web");
    assert_eq!(found.chat_id, "chat-1");
    assert_eq!(found.message_id, "msg-1");
    assert_eq!(found.turn, 0);
    assert!(found.success);
    assert_eq!(found.duration_ms, 42);
}

#[tokio::test]
async fn find_by_chat_id_returns_ordered() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let te1 = test_tool_call("chat-1", "msg-1", 0, "tool_a");
    let te2 = test_tool_call("chat-1", "msg-1", 1, "tool_b");
    let te3 = test_tool_call("chat-2", "msg-2", 0, "tool_c");

    repo.create(&te1).await.unwrap();
    repo.create(&te2).await.unwrap();
    repo.create(&te3).await.unwrap();

    let results = repo.find_by_chat_id("chat-1").await.unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].name, "tool_a");
    assert_eq!(results[1].name, "tool_b");

    let results2 = repo.find_by_chat_id("chat-2").await.unwrap();
    assert_eq!(results2.len(), 1);
    assert_eq!(results2[0].name, "tool_c");
}

#[tokio::test]
async fn find_by_message_id() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let te1 = test_tool_call("chat-1", "msg-1", 0, "tool_a");
    let te2 = test_tool_call("chat-1", "msg-2", 0, "tool_b");

    repo.create(&te1).await.unwrap();
    repo.create(&te2).await.unwrap();

    let results = repo.find_by_message_id("msg-1").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "tool_a");
}

#[tokio::test]
async fn find_pending_by_chat_id() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    use frona::inference::hitl::{Hitl, HitlRequest};

    // Resolved HITL
    let mut te1 = test_tool_call("chat-1", "msg-1", 0, "tool_resolved");
    te1.hitl = Some(Hitl {
        prompt: "Allow credential access?".into(),
        url: "/chats/chat-1".into(),
        request: HitlRequest::Credential {
            query: "test".into(),
            reason: "need creds".into(),
        },
        status: ToolStatus::Resolved,
        response: None,
        delivery: None,
    });
    repo.create(&te1).await.unwrap();

    // Pending HITL
    let mut te2 = test_tool_call("chat-1", "msg-1", 1, "tool_pending");
    te2.hitl = Some(Hitl {
        prompt: "Allow credential access?".into(),
        url: "/chats/chat-1".into(),
        request: HitlRequest::Credential {
            query: "test2".into(),
            reason: "need more creds".into(),
        },
        status: ToolStatus::Pending,
        response: None,
        delivery: None,
    });
    repo.create(&te2).await.unwrap();

    let pending = repo.find_pending_by_chat_id("chat-1").await.unwrap();
    assert!(pending.is_some());
    assert_eq!(pending.unwrap().name, "tool_pending");
}

#[tokio::test]
async fn find_pending_returns_none_when_all_resolved() {
    use frona::inference::hitl::{Hitl, HitlRequest};

    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = test_tool_call("chat-1", "msg-1", 0, "tool_done");
    te.hitl = Some(Hitl {
        prompt: "Allow credential access?".into(),
        url: "/chats/chat-1".into(),
        request: HitlRequest::Credential {
            query: "test".into(),
            reason: "reason".into(),
        },
        status: ToolStatus::Resolved,
        response: None,
        delivery: None,
    });
    repo.create(&te).await.unwrap();

    let pending = repo.find_pending_by_chat_id("chat-1").await.unwrap();
    assert!(pending.is_none());
}

#[tokio::test]
async fn arguments_json_round_trip() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = test_tool_call("chat-1", "msg-1", 0, "complex_tool");
    te.arguments = serde_json::json!({
        "url": "https://example.com",
        "headers": {"Authorization": "Bearer token"},
        "nested": {"deep": [1, 2, 3]}
    });
    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().unwrap();
    assert_eq!(found.arguments["url"], "https://example.com");
    assert_eq!(found.arguments["nested"]["deep"][1], 2);
}

#[tokio::test]
async fn turn_text_round_trip() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = test_tool_call("chat-1", "msg-1", 0, "search_web");
    te.turn_text = Some("Script 3:".into());
    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().unwrap();
    assert_eq!(found.turn_text, Some("Script 3:".to_string()));
}

#[tokio::test]
async fn turn_text_none_round_trip() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let te = test_tool_call("chat-1", "msg-1", 0, "search_web");
    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().unwrap();
    assert!(found.turn_text.is_none());
}

#[tokio::test]
async fn begin_creates_incomplete_record() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let te = ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: "chat-1".to_string(),
        message_id: "msg-1".to_string(),
        turn: 0,
        provider_call_id: "call-1".to_string(),
        name: "web_search".to_string(),
        arguments: serde_json::json!({"query": "rust"}),
        result: String::new(),
        success: false,
        duration_ms: 0,

        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: Some("Searching for info:".into()),
        signature: None,
        turn_reasoning: None,
        created_at: Utc::now(),
    };
    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().unwrap();
    assert_eq!(found.result, "");
    assert!(!found.success);
    assert_eq!(found.duration_ms, 0);
    assert!(found.hitl.is_none());
    assert!(found.system_prompt.is_none());
    assert_eq!(found.turn_text, Some("Searching for info:".to_string()));
}

#[tokio::test]
async fn finish_updates_record() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    // Begin: create incomplete record
    let mut te = test_tool_call("chat-1", "msg-1", 0, "web_search");
    te.result = String::new();
    te.success = false;
    te.duration_ms = 0;
    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    // Finish: update with result
    let mut found = repo.find_by_id(&id).await.unwrap().unwrap();
    found.result = "Search results here".to_string();
    found.success = true;
    found.duration_ms = 150;
    found.system_prompt = Some("injected context".to_string());
    repo.update(&found).await.unwrap();

    let updated = repo.find_by_id(&id).await.unwrap().unwrap();
    assert_eq!(updated.result, "Search results here");
    assert!(updated.success);
    assert_eq!(updated.duration_ms, 150);
    assert_eq!(updated.system_prompt, Some("injected context".to_string()));
}

#[tokio::test]
async fn begin_without_finish_leaves_incomplete() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    // Simulate crash: only begin, never finish
    let te = ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: "chat-1".to_string(),
        message_id: "msg-1".to_string(),
        turn: 0,
        provider_call_id: "call-crash".to_string(),
        name: "checkout_order".to_string(),
        arguments: serde_json::json!({"order_id": "12345"}),
        result: String::new(),
        success: false,
        duration_ms: 0,

        hitl: None,
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc::now(),
    };
    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    // On restart, we can find this incomplete record
    let found = repo.find_by_id(&id).await.unwrap().unwrap();
    assert_eq!(found.name, "checkout_order");
    assert_eq!(found.result, "");
    assert!(!found.success);
    assert_eq!(found.duration_ms, 0);
    assert_eq!(found.arguments["order_id"], "12345");
}
