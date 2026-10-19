use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::super::*;

#[tokio::test]
async fn message_metadata_round_trip_via_send_and_patch() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "mmd", "mmd@example.com", "password123").await;
    let agent = create_agent(&state, &token, "MdAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let chat = create_chat(&state, &token, agent_id, None).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/messages"),
            &token,
            serde_json::json!({
                "content": "hello",
                "metadata": {"channel:external_msg_id": "tg:42"},
            }),
        ))
        .await
        .unwrap();
    let _ = resp.status();

    let app = build_app(state.clone());
    let list = app
        .oneshot(auth_get(&format!("/api/chats/{chat_id}/messages"), &token))
        .await
        .unwrap();
    let json = body_json(list).await;
    let messages = json["messages"].as_array().expect("messages array");
    let user_msg = messages
        .iter()
        .find(|m| m["role"] == "user")
        .expect("user message present");
    assert_eq!(user_msg["metadata"]["channel:external_msg_id"], "tg:42");
    let msg_id = user_msg["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_patch_json(
            &format!("/api/messages/{msg_id}"),
            &token,
            serde_json::json!({
                "metadata": {"channel:external_msg_id": "tg:99", "extra": "x"},
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let patched = body_json(resp).await;
    assert_eq!(patched["metadata"]["channel:external_msg_id"], "tg:99");
    assert_eq!(patched["metadata"]["extra"], "x");
}

#[tokio::test]
async fn list_messages_empty_chat() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "msg-list", "msglist@example.com", "password123").await;
    let agent = create_agent(&state, &token, "ListAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let chat = create_chat(&state, &token, agent_id, Some("ListChat")).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/chats/{chat_id}/messages"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["messages"].is_array());
    assert_eq!(json["has_more"], false);
}

#[tokio::test]
async fn list_messages_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/chats/fake-id/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_messages_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) = register_user(&state, "msg-own", "msgown@example.com", "password123").await;
    let (token_b, _) = register_user(&state, "msg-oth", "msgoth@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "MsgOwn").await;
    let chat = create_chat(&state, &token_a, agent["id"].as_str().unwrap(), None).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/chats/{chat_id}/messages"),
            &token_b,
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "Expected 403 or 404, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn cancel_generation_returns_json() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "msg-cancel", "msgcancel@example.com", "password123").await;
    let agent = create_agent(&state, &token, "CancelAgent").await;
    let chat = create_chat(&state, &token, agent["id"].as_str().unwrap(), None).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/cancel"),
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["cancelled"], false);
}

#[tokio::test]
async fn cancel_generation_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "cancel-own", "cancelown@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "cancel-oth", "canceloth@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "CancelOwn").await;
    let chat = create_chat(&state, &token_a, agent["id"].as_str().unwrap(), None).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/cancel"),
            &token_b,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "Expected 403 or 404, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn resolve_tool_call_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chats/fake-id/tool-calls/resolve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"resolutions": [{"tool_call_id": "te-1", "response": "yes"}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn resolve_tool_call_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) = register_user(
        &state,
        "resolve-own",
        "resolveown@example.com",
        "password123",
    )
    .await;
    let (token_b, _) = register_user(
        &state,
        "resolve-oth",
        "resolveoth@example.com",
        "password123",
    )
    .await;

    let agent = create_agent(&state, &token_a, "ResolveAgent").await;
    let chat = create_chat(&state, &token_a, agent["id"].as_str().unwrap(), None).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/tool-calls/resolve"),
            &token_b,
            serde_json::json!({"resolutions": [{"tool_call_id": "fake-te", "response": "yes"}]}),
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "Expected 403 or 404, got {}",
        resp.status()
    );
}

/// Reproduces a regression where resolving multiple HITLs in a single
/// `POST /tool-calls/resolve` request fails to resume the agent loop.
///
/// Setup mirrors the user-reported case: a paused agent message with
/// three `ask_user_question` HITL tool calls all in `Pending` status. The FE
/// submits all three resolutions in one body. The test asserts that after
/// resolution, every HITL is in `Resolved` status AND the message leaves
/// `Executing` (proving the resolve handler's `tokio::spawn` dispatch — either
/// `task_executor.run_task_by_id` or `harness.resume` — actually fired and ran
/// inference to completion or failure).
///
/// If the dispatch never fires (or both spawn paths cancel each other via
/// `active_sessions.register`), the message stays in `Executing` forever —
/// matching the user's observation that nothing happens after answering.
#[tokio::test]
async fn batched_resolve_resumes_agent_loop() {
    use frona::chat::message::models::MessageStatus;
    use frona::core::repository::Repository;
    use frona::db::repo::generic::SurrealRepo;
    use frona::inference::hitl::{Hitl, HitlRequest};
    use frona::inference::tool_call::{ToolCall, ToolStatus};

    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "batched", "batched@example.com", "password123").await;

    let agent = create_agent(&state, &token, "BatchedAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let chat = create_chat(&state, &token, agent_id, None).await;
    let chat_id = chat["id"].as_str().unwrap().to_string();

    // Create a Paused agent message — the unit the HITL barrier guards.
    // Production code flips Executing → Paused via `pause_agent_message`
    // when the loop hits an `ExternalToolPending` outcome; we replicate that
    // here by creating Executing and patching to Paused.
    let agent_msg = state
        .chat_service
        .create_executing_agent_message(&chat_id, agent_id)
        .await
        .unwrap();
    let agent_msg_id = agent_msg.id.clone();
    {
        let msg_repo: SurrealRepo<frona::chat::message::models::Message> =
            SurrealRepo::new(state.db.clone());
        let mut m = msg_repo.find_by_id(&agent_msg_id).await.unwrap().unwrap();
        m.status = Some(MessageStatus::Paused);
        msg_repo.update(&m).await.unwrap();
    }

    // Insert three pending HITL tool calls on that message. We use
    // `ask_user_question` (registered under NotifyHumanTool) so resume's
    // `find_tool_for_resume` lookup succeeds and `on_resume` runs cleanly.
    let tc_repo: SurrealRepo<ToolCall> = SurrealRepo::new(state.db.clone());
    let mut tc_ids = Vec::new();
    for i in 0..3u32 {
        let id = frona::core::repository::new_id();
        tc_ids.push(id.clone());
        let te = ToolCall {
            id,
            chat_id: chat_id.clone(),
            message_id: agent_msg_id.clone(),
            turn: i,
            provider_call_id: format!("call-{i}"),
            name: "ask_user_question".into(),
            arguments: serde_json::json!({"question": format!("Q{i}")}),
            result: String::new(),
            success: false,
            duration_ms: 0,
            hitl: Some(Hitl {
                prompt: format!("Q{i}?"),
                url: format!("/chats/{chat_id}"),
                request: HitlRequest::Question {
                    options: vec!["yes".into(), "no".into()],
                },
                status: ToolStatus::Pending,
                response: None,
                delivery: None,
            }),
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            signature: None,
            turn_reasoning: None,
            created_at: chrono::Utc::now(),
        };
        tc_repo.create(&te).await.unwrap();
    }

    // POST all three resolutions in a single batch — matches FE behavior.
    let body = serde_json::json!({
        "resolutions": tc_ids.iter().map(|id| serde_json::json!({
            "tool_call_id": id,
            "hitl_response": { "type": "Choice", "data": "yes" },
        })).collect::<Vec<_>>(),
    });

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/tool-calls/resolve"),
            &token,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "resolve POST failed");

    // The synchronous part of /resolve must have cleared the HITL barrier:
    // every tool_call's hitl.status is now Resolved (not Pending).
    let tcs = state
        .chat_service
        .get_tool_calls_by_message(&agent_msg_id)
        .await
        .unwrap();
    assert!(
        tcs.iter().all(|t| t
            .hitl
            .as_ref()
            .is_some_and(|h| h.status != ToolStatus::Pending)),
        "all HITLs should be resolved synchronously"
    );

    // The async part: the dispatch (`harness.resume` or `run_task_by_id`) is
    // `tokio::spawn`-ed; give it room to run. With no model providers
    // registered, inference will fail fast and `fail_agent_message` will flip
    // status to Failed. Either way the message MUST leave Executing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let final_status = loop {
        let msg = state
            .chat_service
            .find_message(&agent_msg_id)
            .await
            .unwrap()
            .expect("message");
        let status = msg.status.clone();
        if !matches!(status, Some(MessageStatus::Executing)) {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            break status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    assert!(
        !matches!(final_status, Some(MessageStatus::Executing)),
        "agent loop never resumed — message stuck in Executing (got {:?})",
        final_status,
    );
}

#[tokio::test]
async fn send_message_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chats/fake-id/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"content": "hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
