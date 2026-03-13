use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::super::*;

// ---------------------------------------------------------------------------
// List messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_messages_empty_chat() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "msg-list", "msglist@example.com", "password123").await;
    let agent = create_agent(&state, &token, "ListAgent").await;
    let agent_id = agent["id"].as_str().unwrap();
    let chat = create_chat(&state, &token, agent_id, Some("ListChat")).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(
            &format!("/api/chats/{chat_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json.is_array());
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
    let (token_a, _) =
        register_user(&state, "msg-own", "msgown@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "msg-oth", "msgoth@example.com", "password123").await;

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

// ---------------------------------------------------------------------------
// Cancel generation
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Send message — auth checks
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Resolve tool message — auth checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_tool_message_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chats/fake-id/messages/msg-1/resolve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"response": "yes"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn resolve_tool_message_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "resolve-own", "resolveown@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "resolve-oth", "resolveoth@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "ResolveAgent").await;
    let chat = create_chat(&state, &token_a, agent["id"].as_str().unwrap(), None).await;
    let chat_id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            &format!("/api/chats/{chat_id}/messages/fake-msg/resolve"),
            &token_b,
            serde_json::json!({"response": "yes"}),
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "Expected 403 or 404, got {}",
        resp.status()
    );
}
