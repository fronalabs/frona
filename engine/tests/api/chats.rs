use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn create_chat_returns_json() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "chatuser", "chatuser@example.com", "password123").await;
    let agent = create_agent(&state, &token, "ChatAgent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let chat = create_chat(&state, &token, agent_id, Some("Hello")).await;
    assert!(chat["id"].is_string());
    assert_eq!(chat["agent_id"], agent_id);
    assert_eq!(chat["title"], "Hello");
}

#[tokio::test]
async fn create_chat_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chats")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"agent_id": "fake"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_chats_returns_only_own() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "chat-a", "chata@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "chat-b", "chatb@example.com", "password123").await;

    let agent_a = create_agent(&state, &token_a, "AgA").await;
    let agent_b = create_agent(&state, &token_b, "AgB").await;

    create_chat(&state, &token_a, agent_a["id"].as_str().unwrap(), None).await;
    create_chat(&state, &token_b, agent_b["id"].as_str().unwrap(), None).await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/chats", &token_a))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn get_chat_by_id() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "getchat", "getchat@example.com", "password123").await;
    let agent = create_agent(&state, &token, "GC").await;
    let chat = create_chat(&state, &token, agent["id"].as_str().unwrap(), Some("MyChat")).await;
    let id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/chats/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["title"], "MyChat");
}

#[tokio::test]
async fn get_chat_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "chatown", "chatown@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "chatoth", "chatoth@example.com", "password123").await;

    let agent = create_agent(&state, &token_a, "CO").await;
    let chat = create_chat(&state, &token_a, agent["id"].as_str().unwrap(), None).await;
    let id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/chats/{id}"), &token_b))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::FORBIDDEN,
        "Expected 404 or 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn update_chat_title() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "upchat", "upchat@example.com", "password123").await;
    let agent = create_agent(&state, &token, "UC").await;
    let chat = create_chat(&state, &token, agent["id"].as_str().unwrap(), Some("Old")).await;
    let id = chat["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            &format!("/api/chats/{id}"),
            &token,
            serde_json::json!({"title": "New Title"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["title"], "New Title");
}

#[tokio::test]
async fn delete_chat() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "delchat", "delchat@example.com", "password123").await;
    let agent = create_agent(&state, &token, "DC").await;
    let chat = create_chat(&state, &token, agent["id"].as_str().unwrap(), None).await;
    let id = chat["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(&format!("/api/chats/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get(&format!("/api/chats/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archive_and_unarchive_chat() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "archuser", "archuser@example.com", "password123").await;
    let agent = create_agent(&state, &token, "AR").await;
    let chat = create_chat(&state, &token, agent["id"].as_str().unwrap(), Some("Arch")).await;
    let id = chat["id"].as_str().unwrap();

    // Archive
    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/chats/{id}/archive"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["archived_at"].is_string());

    // Should not appear in main list
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_get("/api/chats", &token))
        .await
        .unwrap();
    let list = body_json(resp).await;
    assert!(
        list.as_array().unwrap().iter().all(|c| c["id"] != id),
        "Archived chat should not appear in main list"
    );

    // Should appear in archived list
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_get("/api/chats/archived", &token))
        .await
        .unwrap();
    let archived = body_json(resp).await;
    assert!(archived.as_array().unwrap().iter().any(|c| c["id"] == id));

    // Unarchive
    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/chats/{id}/unarchive"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["archived_at"].is_null());

    // Should be back in main list
    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/chats", &token))
        .await
        .unwrap();
    let list = body_json(resp).await;
    assert!(list.as_array().unwrap().iter().any(|c| c["id"] == id));
}
