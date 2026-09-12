use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;

async fn create_local_item(state: &AppState, token: &str, name: &str) -> serde_json::Value {
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/vaults/local/items",
            token,
            serde_json::json!({
                "type": "ApiKey",
                "name": name,
                "api_key": "sk-test-key-123"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "create_local_item({name}) failed"
    );
    body_json(resp).await
}

#[tokio::test]
async fn create_local_item_api_key() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "vault-create",
        "vaultcreate@example.com",
        "password123",
    )
    .await;

    let item = create_local_item(&state, &token, "MyKey").await;
    assert!(item["id"].is_string());
    assert_eq!(item["name"], "MyKey");
}

#[tokio::test]
async fn create_local_item_username_password() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "vault-up", "vaultup@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/vaults/local/items",
            &token,
            serde_json::json!({
                "type": "UsernamePassword",
                "name": "MyLogin",
                "username": "admin",
                "password": "secret"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "MyLogin");
}

#[tokio::test]
async fn create_local_item_browser_profile() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "vault-bp", "vaultbp@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/vaults/local/items",
            &token,
            serde_json::json!({
                "type": "BrowserProfile",
                "name": "Chrome"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "Chrome");
}

#[tokio::test]
async fn create_local_item_without_auth_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/vaults/local/items")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"type": "ApiKey", "name": "X", "api_key": "k"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_local_items_returns_only_own() {
    let (state, _tmp) = test_app_state().await;
    let (token_a, _) = register_user(&state, "vault-a", "vaulta@example.com", "password123").await;
    let (token_b, _) = register_user(&state, "vault-b", "vaultb@example.com", "password123").await;

    create_local_item(&state, &token_a, "KeyA").await;
    create_local_item(&state, &token_b, "KeyB").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/vaults/local/items", &token_a))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "KeyA");
}

#[tokio::test]
async fn update_local_item() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "vault-upd", "vaultupd@example.com", "password123").await;
    let item = create_local_item(&state, &token, "OldKey").await;
    let id = item["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            &format!("/api/vaults/local/items/{id}"),
            &token,
            serde_json::json!({
                "type": "ApiKey",
                "name": "NewKey"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "NewKey");
}

#[tokio::test]
async fn delete_local_item() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "vault-del", "vaultdel@example.com", "password123").await;
    let item = create_local_item(&state, &token, "DeleteMe").await;
    let id = item["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(
            &format!("/api/vaults/local/items/{id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["deleted"], true);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/vaults/local/items", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_connections_includes_only_builtin_vaults_before_personal_creation() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "vault-conn", "vaultconn@example.com", "password123").await;

    let app = build_app(state);
    let resp = app.oneshot(auth_get("/api/vaults", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let connections = json.as_array().unwrap();
    assert_eq!(connections.len(), 2);
    assert!(connections.iter().all(|c| c["system_managed"] == true));
    assert!(
        connections
            .iter()
            .any(|c| c["id"] == "managed" && c["provider"] == "managed")
    );
    assert!(connections.iter().any(|c| c["id"] == "local"));
}

#[tokio::test]
async fn list_grants_empty() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "vault-grants",
        "vaultgrants@example.com",
        "password123",
    )
    .await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/vaults/grants", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn create_grant_returns_json() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(
        &state,
        "vault-grant",
        "vaultgrant@example.com",
        "password123",
    )
    .await;

    let item = create_local_item(&state, &token, "test-item").await;
    let item_id = item["id"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/vaults/grants",
            &token,
            serde_json::json!({
                "principal": {"kind": "agent", "id": "test-agent"},
                "connection_id": "local",
                "vault_item_id": item_id,
                "query": "TEST",
                "target": {"Prefix": {"env_var_prefix": "TEST"}}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["id"].is_string());
    assert_eq!(json["connection_id"], "local");
    assert_eq!(json["vault_item_id"], item_id);
    assert_eq!(json["query"], "TEST");
    assert_eq!(json["principal"]["kind"], "agent");
    assert_eq!(json["principal"]["id"], "test-agent");
}

#[tokio::test]
async fn vault_endpoints_reject_no_auth() {
    let (state, _tmp) = test_app_state().await;

    let cases: Vec<(&str, &str)> = vec![
        ("GET", "/api/vaults"),
        ("POST", "/api/vaults"),
        ("GET", "/api/vaults/local/items"),
        ("POST", "/api/vaults/local/items"),
        ("GET", "/api/vaults/grants"),
    ];

    for (method, uri) in cases {
        let app = build_app(state.clone());
        let body = if method == "POST" {
            Body::from("{}")
        } else {
            Body::empty()
        };
        let mut builder = Request::builder().method(method).uri(uri);
        if method == "POST" {
            builder = builder.header("content-type", "application/json");
        }
        let req = builder.body(body).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} should return 401 without auth"
        );
    }
}
