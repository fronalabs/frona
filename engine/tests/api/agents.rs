use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn create_agent_returns_json() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "agentuser", "agent@example.com", "password123").await;

    let json = create_agent(&state, &token, "MyAgent").await;
    assert_eq!(json["name"], "MyAgent");
    assert!(json["id"].is_string());
}

#[tokio::test]
async fn create_agent_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"name": "X", "description": "X"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_agents_returns_only_own() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "agent-a", "agenta@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "agent-b", "agentb@example.com", "password123").await;

    create_agent(&state, &token_a, "AgentA").await;
    create_agent(&state, &token_b, "AgentB").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/agents", &token_a))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let agents = json.as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["name"], "AgentA");
}

#[tokio::test]
async fn list_agents_includes_chat_count() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "chatcount", "chatcount@example.com", "password123").await;

    let agent = create_agent(&state, &token, "CountMe").await;
    let agent_id = agent["id"].as_str().unwrap();

    create_chat(&state, &token, agent_id, Some("Chat1")).await;
    create_chat(&state, &token, agent_id, Some("Chat2")).await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/agents", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let agents = json.as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["chat_count"], 2);
}

#[tokio::test]
async fn get_agent_by_id() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "getagent", "getagent@example.com", "password123").await;
    let agent = create_agent(&state, &token, "GetMe").await;
    let id = agent["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/agents/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "GetMe");
}

#[tokio::test]
async fn get_agent_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "owner-a", "ownera@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "other-b", "otherb@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "Private").await;
    let id = agent["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/agents/{id}"), &token_b))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::FORBIDDEN,
        "Expected 404 or 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn update_agent() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "updateagent", "updateagent@example.com", "password123").await;
    let agent = create_agent(&state, &token, "Before").await;
    let id = agent["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            &format!("/api/agents/{id}"),
            &token,
            serde_json::json!({"name": "After", "description": "Updated"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "After");
}

#[tokio::test]
async fn delete_agent_then_get_returns_404() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "delagent", "delagent@example.com", "password123").await;
    let agent = create_agent(&state, &token, "ToDelete").await;
    let id = agent["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(&format!("/api/agents/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/agents/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_agent_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "del-owner", "delowner@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "del-other", "delother@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "NoDelete").await;
    let id = agent["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_delete(&format!("/api/agents/{id}"), &token_b))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::FORBIDDEN,
        "Expected 404 or 403, got {}",
        resp.status()
    );
}
