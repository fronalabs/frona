use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn create_space_returns_json() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "spaceuser", "spaceuser@example.com", "password123").await;
    let json = create_space(&state, &token, "MySpace").await;
    assert!(json["id"].is_string());
    assert_eq!(json["name"], "MySpace");
}

#[tokio::test]
async fn create_space_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/spaces")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"name": "X"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_spaces_returns_only_own() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "space-a", "spacea@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "space-b", "spaceb@example.com", "password123").await;

    create_space(&state, &token_a, "SpaceA").await;
    create_space(&state, &token_b, "SpaceB").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/spaces", &token_a))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let spaces = json.as_array().unwrap();
    assert_eq!(spaces.len(), 1);
    assert_eq!(spaces[0]["name"], "SpaceA");
}

#[tokio::test]
async fn update_space() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "upspace", "upspace@example.com", "password123").await;
    let space = create_space(&state, &token, "Before").await;
    let id = space["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            &format!("/api/spaces/{id}"),
            &token,
            serde_json::json!({"name": "After"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "After");
}

#[tokio::test]
async fn delete_space() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "delspace", "delspace@example.com", "password123").await;
    let space = create_space(&state, &token, "GoAway").await;
    let id = space["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(&format!("/api/spaces/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/spaces", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn delete_space_other_user_returns_error() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) =
        register_user(&state, "sp-owner", "spowner@example.com", "password123").await;
    let (token_b, _) =
        register_user(&state, "sp-other", "spother@example.com", "password123").await;

    let space = create_space(&state, &token_a, "Mine").await;
    let id = space["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_delete(&format!("/api/spaces/{id}"), &token_b))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::FORBIDDEN,
        "Expected 404 or 403, got {}",
        resp.status()
    );
}
