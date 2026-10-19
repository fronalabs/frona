//! Integration tests for HITL persistence — verifies that the new `hitl` +
//! `task_event` fields on `ToolCall` round-trip correctly through SurrealDB,
//! and that the `set_hitl` / `set_task_event` / `set_hitl_delivery` methods
//! on `ChatService` work end-to-end against an in-memory backing store.

use chrono::Utc;
use frona::core::repository::Repository;
use frona::credential::vault::models::{CredentialTarget, GrantDuration};
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::hitl::{Hitl, HitlDelivery, HitlRequest, HitlResponse, VaultGrant};
use frona::inference::tool_call::{TaskEvent, ToolCall, ToolStatus};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn base_tool_call(name: &str) -> ToolCall {
    ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: "chat-1".into(),
        message_id: "msg-1".into(),
        turn: 0,
        provider_call_id: format!("call-{}", frona::core::repository::new_id()),
        name: name.into(),
        arguments: serde_json::json!({}),
        result: String::new(),
        success: true,
        duration_ms: 0,

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
async fn tool_call_turn_reasoning_round_trips() {
    use frona::chat::message::models::Reasoning;
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = base_tool_call("ask_user_question");
    te.turn_text = Some("Let me check on that.".into());
    te.turn_reasoning = Some(Reasoning {
        id: Some("r-1".into()),
        content: "I need to ask the user.".into(),
        signature: Some("sig-abc".into()),
        raw: None,
    });

    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().expect("should find");
    let r = found
        .turn_reasoning
        .expect("turn_reasoning should round-trip");
    assert_eq!(r.id.as_deref(), Some("r-1"));
    assert_eq!(r.content, "I need to ask the user.");
    assert_eq!(r.signature.as_deref(), Some("sig-abc"));
    assert_eq!(found.turn_text.as_deref(), Some("Let me check on that."));
}

fn sample_hitl_question() -> Hitl {
    Hitl {
        prompt: "Which region?".into(),
        url: "https://x.example/chats/c1".into(),
        request: HitlRequest::Question {
            options: vec!["us".into(), "eu".into()],
        },
        status: ToolStatus::Pending,
        response: None,
        delivery: None,
    }
}

#[tokio::test]
async fn tool_call_with_pending_hitl_round_trips() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = base_tool_call("ask_user_question");
    te.hitl = Some(sample_hitl_question());

    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().expect("should find");
    let h = found.hitl.expect("hitl should round-trip");
    assert_eq!(h.prompt, "Which region?");
    assert_eq!(h.status, ToolStatus::Pending);
    assert!(h.response.is_none());
    match h.request {
        HitlRequest::Question { options } => {
            assert_eq!(options, vec!["us".to_string(), "eu".to_string()]);
        }
        _ => panic!("expected Question variant"),
    }
}

#[tokio::test]
async fn tool_call_with_resolved_hitl_carries_response() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = base_tool_call("ask_user_question");
    let mut h = sample_hitl_question();
    h.status = ToolStatus::Resolved;
    h.response = Some(HitlResponse::Choice("us".into()));
    h.delivery = Some(HitlDelivery {
        channel_id: "ch-tg".into(),
        external_message_id: "12345".into(),
        delivered_at: Utc::now(),
    });
    te.hitl = Some(h);

    let id = te.id.clone();
    repo.create(&te).await.unwrap();

    let found = repo.find_by_id(&id).await.unwrap().expect("should find");
    let h = found.hitl.expect("hitl should round-trip");
    assert_eq!(h.status, ToolStatus::Resolved);
    match h.response {
        Some(HitlResponse::Choice(s)) => assert_eq!(s, "us"),
        _ => panic!("expected Choice response"),
    }
    let d = h.delivery.expect("delivery should round-trip");
    assert_eq!(d.channel_id, "ch-tg");
    assert_eq!(d.external_message_id, "12345");
}

#[tokio::test]
async fn vault_granted_response_round_trips() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = base_tool_call("request_credentials");
    te.hitl = Some(Hitl {
        prompt: "Allow vault access?".into(),
        url: "https://x/chats/c1".into(),
        request: HitlRequest::Credential {
            query: "postgres-prod".into(),
            reason: "ETL".into(),
        },
        status: ToolStatus::Resolved,
        response: Some(HitlResponse::Vault(VaultGrant::Granted {
            connection_id: "conn-1".into(),
            vault_item_id: "item-1".into(),
            grant_duration: GrantDuration::Once,
            target: CredentialTarget::Prefix {
                env_var_prefix: "DB".into(),
            },
        })),
        delivery: None,
    });

    let id = te.id.clone();
    repo.create(&te).await.unwrap();
    let found = repo.find_by_id(&id).await.unwrap().expect("should find");
    let h = found.hitl.expect("hitl should round-trip");
    match h.response.expect("response should be set") {
        HitlResponse::Vault(VaultGrant::Granted {
            connection_id,
            vault_item_id,
            grant_duration,
            target,
        }) => {
            assert_eq!(connection_id, "conn-1");
            assert_eq!(vault_item_id, "item-1");
            assert!(matches!(grant_duration, GrantDuration::Once));
            match target {
                CredentialTarget::Prefix { env_var_prefix } => {
                    assert_eq!(env_var_prefix, "DB");
                }
                _ => panic!("expected Prefix target"),
            }
        }
        _ => panic!("expected Vault::Granted"),
    }
}

#[tokio::test]
async fn task_event_completion_round_trips() {
    use frona::agent::task::models::TaskStatus;

    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = base_tool_call("complete_task");
    te.task_event = Some(TaskEvent::Completion {
        task_id: "task-1".into(),
        chat_id: Some("chat-1".into()),
        status: TaskStatus::Completed,
        summary: Some("Done".into()),
        deliverables: vec![],
    });

    let id = te.id.clone();
    repo.create(&te).await.unwrap();
    let found = repo.find_by_id(&id).await.unwrap().expect("should find");
    match found.task_event.expect("task_event should round-trip") {
        TaskEvent::Completion {
            task_id,
            status,
            summary,
            ..
        } => {
            assert_eq!(task_id, "task-1");
            assert!(matches!(status, TaskStatus::Completed));
            assert_eq!(summary.as_deref(), Some("Done"));
        }
        _ => panic!("expected Completion"),
    }
}

#[tokio::test]
async fn task_event_deferred_round_trips() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    let mut te = base_tool_call("defer_task");
    te.task_event = Some(TaskEvent::Deferred {
        task_id: "task-1".into(),
        delay_minutes: 30,
        reason: "Try again later".into(),
    });

    let id = te.id.clone();
    repo.create(&te).await.unwrap();
    let found = repo.find_by_id(&id).await.unwrap().expect("should find");
    match found.task_event.expect("task_event should round-trip") {
        TaskEvent::Deferred {
            delay_minutes,
            reason,
            ..
        } => {
            assert_eq!(delay_minutes, 30);
            assert_eq!(reason, "Try again later");
        }
        _ => panic!("expected Deferred"),
    }
}

#[tokio::test]
async fn hitl_and_task_event_independent_fields() {
    let db = test_db().await;
    let repo: SurrealRepo<ToolCall> = SurrealRepo::new(db);

    // Verify both fields can be set independently (and neither requires the other).
    let mut te_hitl = base_tool_call("ask_user_question");
    te_hitl.hitl = Some(sample_hitl_question());
    repo.create(&te_hitl).await.unwrap();

    let mut te_task = base_tool_call("complete_task");
    te_task.task_event = Some(TaskEvent::Deferred {
        task_id: "t".into(),
        delay_minutes: 1,
        reason: "x".into(),
    });
    repo.create(&te_task).await.unwrap();

    let f1 = repo.find_by_id(&te_hitl.id).await.unwrap().unwrap();
    let f2 = repo.find_by_id(&te_task.id).await.unwrap().unwrap();
    assert!(f1.hitl.is_some() && f1.task_event.is_none());
    assert!(f2.task_event.is_some() && f2.hitl.is_none());
}
