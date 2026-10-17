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
                "handle": "alice",
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
    let cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(cookie.contains("refresh_token="));

    let json = body_json(resp).await;
    assert!(json["token"].is_string());
    assert!(json["user"]["id"].is_string());
    assert_eq!(json["user"]["handle"], "alice");
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
                "handle": "user2",
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
async fn register_duplicate_handle_returns_400() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "dupuser", "first@example.com", "password123").await;

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "handle": "dupuser",
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
                "handle": "shortpw",
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
    assert_eq!(json["user"]["handle"], "loginuser");
}

#[tokio::test]
async fn login_by_handle() {
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
    assert_eq!(json["user"]["handle"], "namelogin");
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
    assert_eq!(json["handle"], "meuser");
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
    let (token, _) = register_user(&state, "setupuser", "setup@example.com", "password123").await;

    state
        .set_runtime_config("setup_completed", "true")
        .await
        .unwrap();

    let app = build_app(state);
    let resp = app.oneshot(auth_get("/api/auth/me", &token)).await.unwrap();
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
    let resp = app.oneshot(auth_get("/api/auth/me", &token)).await.unwrap();
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
                "handle": "refreshuser",
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
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/refresh")
        .header("cookie", cookie_value)
        .body(Body::empty())
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();

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
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/refresh")
        .body(Body::empty())
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn change_handle_succeeds() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "oldname", "chname@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            "/api/auth/handle",
            &token,
            serde_json::json!({"handle": "newname"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["user"]["handle"], "newname");
    assert!(json["token"].is_string());
}

#[tokio::test]
async fn change_handle_duplicate_returns_400() {
    let (state, _tmp) = test_app_state().await;
    register_user(&state, "taken-name", "taken@example.com", "password123").await;
    let (token, _) = register_user(&state, "changer", "changer@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            "/api/auth/handle",
            &token,
            serde_json::json!({"handle": "taken-name"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn change_handle_same_returns_400() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "samename", "same@example.com", "password123").await;

    let app = build_app(state);
    let resp = app
        .oneshot(auth_put_json(
            "/api/auth/handle",
            &token,
            serde_json::json!({"handle": "samename"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_pat_returns_201() {
    let (state, _tmp) = test_app_state().await;
    let (token, _) = register_user(&state, "patuser", "patuser@example.com", "password123").await;

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
    let (token, _) = register_user(&state, "listpat", "listpat@example.com", "password123").await;

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
    let (token, _) = register_user(&state, "delpat", "delpat@example.com", "password123").await;

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
    let (token, _) = register_user(&state, "patauth", "patauth@example.com", "password123").await;

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
    assert_eq!(json["handle"], "patauth");
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

#[tokio::test]
async fn auth_config_returns_defaults() {
    let (state, _tmp) = test_app_state().await;
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["sso"]["enabled"], false);
    assert_eq!(json["sso"]["disable_local_auth"], false);
    assert_eq!(json["allow_registration"], true);
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
async fn sso_callback_without_provider_redirects_with_error() {
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
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("/login?sso_error="));
}

fn build_disabled_registration_state(state: &AppState) -> AppState {
    let mut new_state = state.clone();
    let mut config = (*new_state.config).clone();
    config.auth.allow_registration = false;
    new_state.config = std::sync::Arc::new(config);
    new_state
}

#[tokio::test]
async fn register_returns_403_when_disabled() {
    let (state, _tmp) = test_app_state().await;
    // Seed a user so we're not in the "no users" startup-precondition case.
    register_user(&state, "first", "first@example.com", "password123").await;

    let disabled = build_disabled_registration_state(&state);
    let app = build_app(disabled);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "handle": "newuser",
                "email": "new@example.com",
                "name": "New",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn register_returns_403_when_disabled_even_with_no_users() {
    let (state, _tmp) = test_app_state().await;
    let disabled = build_disabled_registration_state(&state);
    let app = build_app(disabled);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "handle": "newuser",
                "email": "new@example.com",
                "name": "New",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    // No bootstrap escape — even with zero users, the route refuses.
    // (The startup precondition is what gates whether the server reaches this state at all.)
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

fn build_sso_only_state(state: &AppState) -> AppState {
    let mut sso_state = state.clone();
    let mut config = (*sso_state.config).clone();
    config.sso.disable_local_auth = true;
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
                "handle": "ssouser",
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
                "handle": "secureuser",
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
    let cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(cookie.contains("Secure"));
}

#[tokio::test]
async fn first_password_register_becomes_admin() {
    let (state, _tmp) = test_app_state().await;
    // Seed the admins group (normally done at server startup).
    state.user_group_service.seed_built_in().await.unwrap();

    let (_, user_id) =
        register_user(&state, "firstadmin", "first@example.com", "password123").await;

    let user = state
        .user_service
        .find_by_id(&user_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        user.groups.iter().any(|g| g == "admins"),
        "first registered user should be promoted to admins, got groups: {:?}",
        user.groups
    );
}

#[tokio::test]
async fn second_register_is_not_admin() {
    let (state, _tmp) = test_app_state().await;
    state.user_group_service.seed_built_in().await.unwrap();

    register_user(&state, "firstadmin", "first@example.com", "password123").await;
    let (_, second_id) = register_user(&state, "second", "second@example.com", "password123").await;

    let user = state
        .user_service
        .find_by_id(&second_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !user.groups.iter().any(|g| g == "admins"),
        "second registered user should not be admin, got groups: {:?}",
        user.groups
    );
}

#[tokio::test]
async fn ensure_admin_invariant_is_idempotent() {
    let (state, _tmp) = test_app_state().await;
    state.user_group_service.seed_built_in().await.unwrap();
    register_user(&state, "onlyone", "only@example.com", "password123").await;

    // Run twice in a row; second call should be a no-op (idempotent).
    state.user_service.ensure_admin_invariant().await.unwrap();
    state.user_service.ensure_admin_invariant().await.unwrap();

    let users = state.user_service.list_all(true).await.unwrap();
    let admin_count = users
        .iter()
        .filter(|u| u.deactivated_at.is_none() && u.groups.iter().any(|g| g == "admins"))
        .count();
    assert_eq!(admin_count, 1);
}

#[tokio::test]
async fn startup_promotes_oldest_active_user_when_no_admin() {
    use chrono::Utc;
    use frona::auth::User as UserModel;
    use frona::core::repository::new_id;

    let (state, _tmp) = test_app_state().await;
    state.user_group_service.seed_built_in().await.unwrap();

    // Insert two users via the repo (bypass the register flow so neither is auto-admin).
    let now = Utc::now();
    let older = UserModel {
        id: new_id(),
        handle: frona::handle!("older"),
        email: "older@example.com".into(),
        name: "Older".into(),
        password_hash: "x".into(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
        created_at: now,
        updated_at: now,
    };
    let newer = UserModel {
        id: new_id(),
        handle: frona::handle!("newer"),
        email: "newer@example.com".into(),
        name: "Newer".into(),
        password_hash: "x".into(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
        created_at: now + chrono::Duration::seconds(1),
        updated_at: now + chrono::Duration::seconds(1),
    };
    state.user_service.create(&older).await.unwrap();
    state.user_service.create(&newer).await.unwrap();

    // Pre-condition: no admin.
    let users_before = state.user_service.list_all(true).await.unwrap();
    assert!(
        users_before
            .iter()
            .all(|u| !u.groups.iter().any(|g| g == "admins")),
        "expected no admins before repair"
    );

    // Run the invariant repair (this is what main.rs does at boot).
    state.user_service.ensure_admin_invariant().await.unwrap();

    let promoted = state
        .user_service
        .find_by_id(&older.id)
        .await
        .unwrap()
        .unwrap();
    assert!(promoted.groups.iter().any(|g| g == "admins"));
    let untouched = state
        .user_service
        .find_by_id(&newer.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!untouched.groups.iter().any(|g| g == "admins"));
}

#[tokio::test]
async fn login_refuses_deactivated_user() {
    let (state, _tmp) = test_app_state().await;
    state.user_group_service.seed_built_in().await.unwrap();
    // First user becomes admin via bootstrap; the DB events refuse deactivating the
    // last admin, so we need a second non-admin user to deactivate.
    register_user(&state, "firstadmin", "admin@example.com", "password123").await;
    let (_, user_id) = register_user(
        &state,
        "tobedisabled",
        "disabled@example.com",
        "password123",
    )
    .await;

    state.user_service.deactivate(&user_id).await.unwrap();

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "identifier": "disabled@example.com",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("deactivated")
    );
}

#[tokio::test]
async fn refresh_refuses_deactivated_user() {
    let (state, _tmp) = test_app_state().await;
    state.user_group_service.seed_built_in().await.unwrap();
    // First user is auto-admin; we need a second non-admin to deactivate.
    register_user(&state, "firstadmin", "admin@example.com", "password123").await;
    let (_, user_id) =
        register_user(&state, "refreshuser", "refresh@example.com", "password123").await;

    // Capture the refresh cookie from registration.
    let app = build_app(state.clone());
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "identifier": "refresh@example.com",
                "password": "password123",
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let login_resp = app.oneshot(req).await.unwrap();
    assert_eq!(login_resp.status(), StatusCode::OK);
    let cookie = login_resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let refresh_cookie = cookie.split(';').next().unwrap().to_string();

    // Deactivate the user, then try to refresh with the captured cookie.
    state.user_service.deactivate(&user_id).await.unwrap();

    let app = build_app(state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/refresh")
        .header("cookie", refresh_cookie)
        .body(Body::empty())
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn competing_refresh_responses_preserve_the_winning_cookie() {
    let (state, _tmp) = test_app_state().await;
    let (_, user_id) =
        register_user(&state, "refresh-race", "race@example.com", "password123").await;
    let user = state
        .user_service
        .find_by_id(&user_id)
        .await
        .unwrap()
        .unwrap();
    let (_, refresh) = state
        .token_service
        .create_session_pair(&state.keypair_service, &user)
        .await
        .unwrap();
    let cookie = format!("refresh_token={refresh}");
    let request = |cookie: &str| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/auth/refresh")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        with_connect_info(&mut request);
        request
    };

    let mut other = state.clone();
    other.token_service = frona::auth::token::service::TokenService::new(
        std::sync::Arc::new(SurrealRepo::new(state.db.clone())),
        frona::auth::jwt::JwtService::new(),
        state.user_service.clone(),
        900,
        604800,
    );
    let (a, b) = tokio::join!(
        build_app(state.clone()).oneshot(request(&cookie)),
        build_app(other).oneshot(request(&cookie)),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(
        usize::from(a.status() == StatusCode::OK) + usize::from(b.status() == StatusCode::OK),
        1
    );
    let (winner, loser) = if a.status() == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    assert!(matches!(
        loser.status(),
        StatusCode::UNAUTHORIZED | StatusCode::INTERNAL_SERVER_ERROR
    ));
    assert!(
        !loser.headers().contains_key("set-cookie"),
        "a delayed loser must not erase the winning cookie"
    );
    let replacement_cookie = winner
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_ne!(replacement_cookie, cookie);
    let access = body_json(winner).await["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let replay = build_app(state.clone())
        .oneshot(request(&cookie))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    assert!(!replay.headers().contains_key("set-cookie"));
    state
        .token_service
        .validate_api(&state.keypair_service, &access)
        .await
        .unwrap();
    let retry = build_app(state)
        .oneshot(request(&replacement_cookie))
        .await
        .unwrap();
    assert_eq!(
        retry.status(),
        StatusCode::OK,
        "the shared winning cookie remains usable for a later retry"
    );
}

#[tokio::test]
async fn refresh_and_logout_report_database_revocation_failures_without_clearing_cookie() {
    let (state, _tmp) = test_app_state().await;
    let (_, user_id) = register_user(
        &state,
        "refresh-failure",
        "failure@example.com",
        "password123",
    )
    .await;
    let user = state
        .user_service
        .find_by_id(&user_id)
        .await
        .unwrap()
        .unwrap();
    let (access, refresh) = state
        .token_service
        .create_session_pair(&state.keypair_service, &user)
        .await
        .unwrap();
    state.db.query("DEFINE EVENT reject_delete ON api_token WHEN $event = 'DELETE' AND $before.name = 'session' THEN { THROW 'injected delete failure'; }")
        .await.unwrap().check().unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/auth/refresh")
        .header("cookie", format!("refresh_token={refresh}"))
        .body(Body::empty())
        .unwrap();
    with_connect_info(&mut request);
    let response = build_app(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!response.headers().contains_key("set-cookie"));

    let response = build_app(state.clone())
        .oneshot(auth_post_json(
            "/api/auth/logout",
            &access,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!response.headers().contains_key("set-cookie"));
    state
        .token_service
        .validate_api(&state.keypair_service, &access)
        .await
        .unwrap();
    state
        .token_service
        .validate_refresh(&state.keypair_service, &refresh)
        .await
        .unwrap();
}
