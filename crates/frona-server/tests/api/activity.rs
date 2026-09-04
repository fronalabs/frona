use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use frona::core::execution::{ExecutionKind, ExecutionSource, ExecutionSourceKind, NewExecution};

use super::*;

fn inference(title: &str, chat_id: &str) -> NewExecution {
    NewExecution {
        title: title.to_string(),
        agent_name: Some("Assistant".to_string()),
        kind: ExecutionKind::Inference,
        action: Some("Generating response".to_string()),
        source: Some(ExecutionSource {
            kind: ExecutionSourceKind::Chat,
            id: Some(chat_id.to_string()),
        }),
        related_chat_ids: vec![chat_id.to_string()],
        can_cancel: true,
    }
}

#[tokio::test]
async fn activity_requires_authentication() {
    let (state, _tmp) = test_app_state().await;
    let response = build_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/activity")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn activity_returns_only_the_current_users_executions() {
    let (state, _tmp) = test_app_state().await;
    let (first_token, first_user_id) = register_user(
        &state,
        "activity-a",
        "activity-a@example.com",
        "password123",
    )
    .await;
    let (_, second_user_id) = register_user(
        &state,
        "activity-b",
        "activity-b@example.com",
        "password123",
    )
    .await;
    let _first = state.execution_registry.start(
        &first_user_id,
        inference("Answering support chat", "chat-1"),
    );
    let _second = state
        .execution_registry
        .start(&second_user_id, inference("Private work", "chat-2"));

    let response = build_app(state)
        .oneshot(auth_get("/api/activity", &first_token))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let executions = json["executions"].as_array().unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0]["title"], "Answering support chat");
    assert_eq!(executions[0]["agentName"], "Assistant");
    assert_eq!(executions[0]["kind"], "inference");
    assert_eq!(executions[0]["status"], "running");
    assert_eq!(executions[0]["source"]["type"], "chat");
    assert_eq!(executions[0]["source"]["id"], "chat-1");
    assert_eq!(
        executions[0]["relatedChatIds"],
        serde_json::json!(["chat-1"])
    );
    assert_eq!(executions[0]["canCancel"], true);
    assert!(executions[0]["startedAt"].is_string());
}

#[tokio::test]
async fn finished_executions_disappear_from_the_snapshot() {
    let (state, _tmp) = test_app_state().await;
    let (token, user_id) = register_user(
        &state,
        "activity-drop",
        "activity-drop@example.com",
        "password123",
    )
    .await;
    let execution = state
        .execution_registry
        .start(&user_id, inference("Temporary work", "chat-1"));
    drop(execution);

    let response = build_app(state)
        .oneshot(auth_get("/api/activity", &token))
        .await
        .unwrap();
    let json = body_json(response).await;

    assert_eq!(json["executions"], serde_json::json!([]));
}
