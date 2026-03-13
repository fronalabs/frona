use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::super::*;

/// Read SSE frames from a response body until timeout, returning accumulated text.
async fn collect_sse_frames(body: Body, timeout_ms: u64) -> String {
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut body = body;
    while let Ok(Some(Ok(frame))) = tokio::time::timeout_at(deadline, body.frame()).await {
        if let Some(data) = frame.data_ref() {
            collected.push_str(&String::from_utf8_lossy(data));
        }
    }
    collected
}

// ---------------------------------------------------------------------------
// Event stream (GET /api/stream) — broadcast SSE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_stream_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn event_stream_returns_sse_content_type() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "sse-ct", "ssect@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/stream", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "Expected text/event-stream, got {ct}"
    );
}

#[tokio::test]
async fn event_stream_receives_chat_message_broadcast() {
    use frona::chat::message::models::{MessageResponse, MessageRole};

    let (state, _tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "sse-msg", "ssemsg@example.com", "password123").await;

    let broadcast = state.broadcast_service.clone();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/stream", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Broadcast a chat message for this user
    let msg = MessageResponse {
        id: "msg-1".into(),
        chat_id: "chat-1".into(),
        role: MessageRole::Agent,
        content: "Hello from SSE".into(),
        agent_id: None,
        tool_calls: None,
        tool_call_id: None,
        tool: None,
        attachments: vec![],
        contact_id: None,
        created_at: chrono::Utc::now(),
    };
    broadcast.broadcast_chat_message(&user_id, "chat-1", msg);

    // Also broadcast a message for a different user — should NOT appear
    let other_msg = MessageResponse {
        id: "msg-2".into(),
        chat_id: "chat-2".into(),
        role: MessageRole::Agent,
        content: "Not for you".into(),
        agent_id: None,
        tool_calls: None,
        tool_call_id: None,
        tool: None,
        attachments: vec![],
        contact_id: None,
        created_at: chrono::Utc::now(),
    };
    broadcast.broadcast_chat_message("other-user-id", "chat-2", other_msg);

    // Broadcast an inference count (goes to all users)
    broadcast.broadcast_inference_count(42);

    let body_text = collect_sse_frames(resp.into_body(), 500).await;

    // Should contain the chat_message event for our user
    assert!(
        body_text.contains("event: chat_message"),
        "Expected chat_message event in SSE stream, got:\n{body_text}"
    );
    assert!(
        body_text.contains("Hello from SSE"),
        "Expected message content in SSE stream"
    );

    // Should contain the inference_count event (broadcast to all)
    assert!(
        body_text.contains("event: inference_count"),
        "Expected inference_count event in SSE stream, got:\n{body_text}"
    );
    assert!(
        body_text.contains("42"),
        "Expected count value in inference_count event"
    );

    // Should NOT contain the other user's message
    assert!(
        !body_text.contains("Not for you"),
        "SSE stream should filter out other users' messages"
    );
}

#[tokio::test]
async fn event_stream_receives_task_update_broadcast() {
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) =
        register_user(&state, "sse-task", "ssetask@example.com", "password123").await;

    let broadcast = state.broadcast_service.clone();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/stream", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    broadcast.broadcast_task_update(
        &user_id,
        "task-1",
        "completed",
        "My Task",
        Some("chat-1"),
        Some("source-chat-1"),
        Some("All done"),
    );

    let body_text = collect_sse_frames(resp.into_body(), 500).await;

    assert!(
        body_text.contains("event: task_update"),
        "Expected task_update event in SSE stream, got:\n{body_text}"
    );
    assert!(
        body_text.contains("My Task"),
        "Expected task title in task_update event"
    );
    assert!(
        body_text.contains("completed"),
        "Expected status in task_update event"
    );
    assert!(
        body_text.contains("All done"),
        "Expected result_summary in task_update event"
    );
}

#[tokio::test]
async fn event_stream_filters_other_user_task_updates() {
    let (state, _tmp) = test_app_state().await;
    let (token, _user_id) =
        register_user(&state, "sse-filt", "ssefilt@example.com", "password123").await;

    let broadcast = state.broadcast_service.clone();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/stream", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Broadcast task update for a different user
    broadcast.broadcast_task_update(
        "some-other-user",
        "task-99",
        "running",
        "Secret Task",
        None,
        None,
        None,
    );

    // Broadcast an inference count so we have at least one event
    broadcast.broadcast_inference_count(1);

    let body_text = collect_sse_frames(resp.into_body(), 500).await;

    assert!(
        !body_text.contains("Secret Task"),
        "SSE stream should not contain other user's task updates"
    );
    assert!(
        body_text.contains("event: inference_count"),
        "Expected inference_count event (broadcast to all)"
    );
}

// ---------------------------------------------------------------------------
// Stream message (POST /api/chats/{id}/messages/stream) — auth checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_message_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chats/fake-id/messages/stream")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"content": "X"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stream_message_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "sse-own", "sseown@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "sse-oth", "sseoth@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "SSEAgent").await;
    let chat = create_chat(&state, &token_a, agent["id"].as_str().unwrap(), None).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/messages/stream"),
            &token_b,
            serde_json::json!({"content": "hijack"}),
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "Expected 403 or 404, got {}",
        resp.status()
    );
}
