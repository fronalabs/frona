use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn register_returns_201_with_token() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "username": "alice",
                "email": "alice@example.com",
                "name": "Alice",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie.contains("refresh_token="));

    let json = body_json(resp).await;
    assert!(json["token"].is_string());
    assert!(json["user"]["id"].is_string());
    assert_eq!(json["user"]["username"], "alice");
}

#[tokio::test]
async fn register_duplicate_email_returns_400() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "user1", "dup@example.com", "password123").await;

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "username": "user2",
                "email": "dup@example.com",
                "name": "User2",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn register_duplicate_username_returns_400() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "dupuser", "first@example.com", "password123").await;

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "username": "dupuser",
                "email": "second@example.com",
                "name": "Dup",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn register_short_password_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "username": "shortpw",
                "email": "short@example.com",
                "name": "Short",
                "password": "abc",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn login_returns_token() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "loginuser", "login@example.com", "password123").await;

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "identifier": "login@example.com",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["token"].is_string());
    assert_eq!(json["user"]["username"], "loginuser");
}

#[tokio::test]
async fn login_by_username() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "namelogin", "namelogin@example.com", "password123").await;

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "identifier": "namelogin",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["user"]["username"], "namelogin");
}

#[tokio::test]
async fn login_wrong_password_returns_401() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "wrongpw", "wrong@example.com", "password123").await;

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "identifier": "wrong@example.com",
                "password": "badpassword",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_returns_user_info() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "meuser", "me@example.com", "password123").await;

    let app = build_app(state);
    let resp = app.oneshot(auth_get("/api/auth/me", &token)).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["username"], "meuser");
    assert_eq!(json["email"], "me@example.com");
}

#[tokio::test]
async fn me_without_token_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/auth/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_with_invalid_token_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/auth/me", "not.a.real.token"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_returns_needs_setup_false_after_setup_completed() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "setupuser", "setup@example.com", "password123").await;

    state
        .set_runtime_config("setup_completed", "true")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/auth/me", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json.get("needs_setup").is_none());
}

#[tokio::test]
async fn invalid_authorization_format_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/auth/me")
                .header("authorization", "NotBearer some-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logout_invalidates_session() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "logoutuser", "logout@example.com", "password123").await;

    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/auth/me", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn refresh_returns_new_token() {
    let (state, _tmp) = test_app_state().await;

    let app = build_app(state.clone());
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "username": "refreshuser",
                "email": "refresh@example.com",
                "name": "Refresh",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    let cookie_header = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let cookie_value = cookie_header.split(';').next().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/refresh")
                .header("cookie", cookie_value)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let new_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(new_cookie.contains("refresh_token="));
    let json = body_json(resp).await;
    assert!(json["token"].is_string());
}

#[tokio::test]
async fn refresh_without_cookie_returns_401() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/refresh")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── Change Username ────────────────────────────────────────────────

#[tokio::test]
async fn change_username_succeeds() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "oldname", "chname@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            "/api/auth/username",
            &token,
            serde_json::json!({"username": "newname"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["user"]["username"], "newname");
    assert!(json["token"].is_string());
}

#[tokio::test]
async fn change_username_duplicate_returns_400() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "taken-name", "taken@example.com", "password123").await;
    let (token, _) =
        register_user(&state, "changer", "changer@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            "/api/auth/username",
            &token,
            serde_json::json!({"username": "taken-name"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn change_username_same_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "samename", "same@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            "/api/auth/username",
            &token,
            serde_json::json!({"username": "samename"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── PAT Management ────────────────────────────────────────────────

#[tokio::test]
async fn create_pat_returns_201() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "patuser", "patuser@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/auth/tokens",
            &token,
            serde_json::json!({"name": "my-token", "expires_in_days": 30}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert!(json["token"].is_string());
    assert_eq!(json["name"], "my-token");
    assert!(json["prefix"].is_string());
    assert!(json["expires_at"].is_string());
}

#[tokio::test]
async fn list_pats_returns_created_tokens() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "listpat", "listpat@example.com", "password123").await;

    for name in ["token-a", "token-b"] {
        let app = build_app(state.clone());
        let resp = app
            .oneshot(auth_post_json(
                "/api/auth/tokens",
                &token,
                serde_json::json!({"name": name}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/auth/tokens", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn delete_pat_returns_204() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "delpat", "delpat@example.com", "password123").await;

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/auth/tokens",
            &token,
            serde_json::json!({"name": "to-delete"}),
        ))
        .await
        .unwrap();
    let pat = body_json(resp).await;
    let pat_id = pat["id"].as_str().unwrap();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_delete(&format!("/api/auth/tokens/{pat_id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/auth/tokens", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn pat_can_authenticate() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "patauth", "patauth@example.com", "password123").await;

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/auth/tokens",
            &token,
            serde_json::json!({"name": "auth-pat"}),
        ))
        .await
        .unwrap();
    let pat = body_json(resp).await;
    let pat_token = pat["token"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/auth/me", pat_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["username"], "patauth");
}

#[tokio::test]
async fn pat_cannot_create_another_pat() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "pat-no-create", "patnc@example.com", "password123").await;

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/auth/tokens",
            &token,
            serde_json::json!({"name": "first-pat"}),
        ))
        .await
        .unwrap();
    let pat = body_json(resp).await;
    let pat_token = pat["token"].as_str().unwrap();

    let app = build_app(state);
    let resp = app
        .oneshot(auth_post_json(
            "/api/auth/tokens",
            pat_token,
            serde_json::json!({"name": "second-pat"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_with_pat_deletes_token() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) =
        register_user(&state, "patlogout", "patlogout@example.com", "password123").await;

    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/auth/tokens",
            &token,
            serde_json::json!({"name": "logout-pat"}),
        ))
        .await
        .unwrap();
    let pat = body_json(resp).await;
    let pat_token = pat["token"].as_str().unwrap().to_string();

    let app = build_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header("authorization", format!("Bearer {pat_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let app = build_app(state);
    let resp = app
        .oneshot(auth_get("/api/auth/me", &pat_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── SSO ────────────────────────────────────────────────────────────

#[tokio::test]
async fn sso_status_returns_disabled_by_default() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/auth/sso")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["enabled"], false);
    assert_eq!(json["sso_only"], false);
}

#[tokio::test]
async fn sso_authorize_without_provider_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/auth/sso/authorize")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sso_callback_without_provider_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/auth/sso/callback?code=test&state=test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── SSO-Only Mode ──────────────────────────────────────────────────

fn build_sso_only_state(state: &AppState) -> AppState {
    let mut sso_state = state.clone();
    let mut config = (*sso_state.config).clone();
    config.sso.only = true;
    sso_state.config = std::sync::Arc::new(config);
    sso_state
}

#[tokio::test]
async fn register_with_sso_only_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let sso_state = build_sso_only_state(&state);
    let app = build_app(sso_state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "username": "ssouser",
                "email": "sso@example.com",
                "name": "SSO",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn login_with_sso_only_returns_400() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "ssologin", "ssologin@example.com", "password123").await;

    let sso_state = build_sso_only_state(&state);
    let app = build_app(sso_state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "identifier": "ssologin@example.com",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── Secure Cookie ──────────────────────────────────────────────────

#[tokio::test]
async fn register_with_https_base_url_sets_secure_cookie() {
    let (state, _tmp) = test_app_state().await;
    let mut https_state = state.clone();
    let mut config = (*https_state.config).clone();
    config.server.base_url = Some("https://example.com".to_string());
    https_state.config = std::sync::Arc::new(config);

    let app = build_app(https_state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "username": "secureuser",
                "email": "secure@example.com",
                "name": "Secure",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie.contains("Secure"));
}
