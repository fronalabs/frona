use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use frona::app::models::{App, AppStatus};
use frona::auth::User;
use frona::auth::jwt::JwtService;
use frona::auth::token::models::TokenType;
use frona::auth::token::service::{CreateTokenRequest, CreatedToken};
use frona::core::Principal;
use frona::core::repository::Repository;
use tower::ServiceExt;

use super::*;

const TOKEN_TYPES: [TokenType; 4] = [
    TokenType::Access,
    TokenType::Pat,
    TokenType::Ephemeral,
    TokenType::Refresh,
];

#[derive(Clone, Copy, Debug)]
enum Gate {
    UserApi,
    PrincipalApi,
    File,
    AppBearer,
    AppCookie,
    AppBootstrap,
    Voice,
    Refresh,
}

const GATES: [Gate; 8] = [
    Gate::UserApi,
    Gate::PrincipalApi,
    Gate::File,
    Gate::AppBearer,
    Gate::AppCookie,
    Gate::AppBootstrap,
    Gate::Voice,
    Gate::Refresh,
];

impl Gate {
    fn accepts(self, kind: TokenType) -> bool {
        match self {
            Self::UserApi | Self::PrincipalApi | Self::File | Self::AppBearer => {
                matches!(
                    kind,
                    TokenType::Access | TokenType::Pat | TokenType::Ephemeral
                )
            }
            Self::AppCookie | Self::Voice => kind == TokenType::Access,
            Self::AppBootstrap | Self::Refresh => kind == TokenType::Refresh,
        }
    }

    async fn request(self, state: &AppState, token: &str) -> Response {
        let (method, path, header, value) = match self {
            Self::UserApi => (
                "GET",
                "/api/auth/me".into(),
                "authorization",
                format!("Bearer {token}"),
            ),
            Self::PrincipalApi => (
                "GET",
                "/api/mcp/bridge/servers".into(),
                "authorization",
                format!("Bearer {token}"),
            ),
            Self::File => (
                "GET",
                "/api/files/user/token-matrix/readme.txt".into(),
                "authorization",
                format!("Bearer {token}"),
            ),
            Self::AppBearer => (
                "GET",
                "/apps/dashboard/".into(),
                "authorization",
                format!("Bearer {token}"),
            ),
            Self::AppCookie => (
                "GET",
                "/apps/dashboard/".into(),
                "cookie",
                format!("app_session={token}"),
            ),
            Self::AppBootstrap => (
                "GET",
                "/api/auth/apps?redirect=/apps/dashboard/".into(),
                "cookie",
                format!("refresh_token={token}"),
            ),
            Self::Voice => (
                "POST",
                format!("/api/voice/twilio/callback?token={token}"),
                "content-type",
                "application/json".into(),
            ),
            Self::Refresh => (
                "POST",
                "/api/auth/refresh".into(),
                "cookie",
                format!("refresh_token={token}"),
            ),
        };
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header, value)
            .body(Body::empty())
            .unwrap();
        with_connect_info(&mut request);
        // A fresh router keeps the refresh rate limiter independent per matrix case.
        let app = if matches!(self, Self::Voice) {
            routes::voice::router().with_state(state.clone())
        } else {
            build_app(state.clone())
        };
        app.oneshot(request).await.unwrap()
    }

    fn assert_response(self, response: &Response, accepted: bool, case: &str) {
        let expected = match (self, accepted) {
            (Self::AppBootstrap | Self::AppBearer | Self::AppCookie, false)
            | (Self::AppBootstrap, true) => StatusCode::TEMPORARY_REDIRECT,
            (Self::Voice, false) => StatusCode::FORBIDDEN,
            (_, false) => StatusCode::UNAUTHORIZED,
            (_, true) => StatusCode::OK,
        };
        assert_eq!(response.status(), expected, "{case}: {self:?}");
        if matches!(self, Self::AppBootstrap) {
            assert_eq!(
                response.headers().contains_key("set-cookie"),
                accepted,
                "{case}: bootstrap cookie"
            );
            let location = response.headers()["location"].to_str().unwrap();
            if accepted {
                assert_eq!(location, "/apps/dashboard/");
            } else {
                assert!(location.contains("/login?"), "{case}: {location}");
            }
        } else if matches!(self, Self::AppBearer | Self::AppCookie) && !accepted {
            assert!(
                response.headers()["location"]
                    .to_str()
                    .unwrap()
                    .starts_with("/api/auth/apps?")
            );
        }
        if matches!(self, Self::Refresh) && !accepted {
            assert!(
                !response.headers().contains_key("set-cookie"),
                "failed refresh must preserve the current cookie"
            );
        }
    }
}

async fn fixture() -> (AppState, tempfile::TempDir, User) {
    let (state, tmp) = test_app_state().await;
    let (access, user_id) =
        register_user(&state, "token-matrix", "matrix@example.com", "password123").await;
    let user = state
        .user_service
        .find_by_id(&user_id)
        .await
        .unwrap()
        .unwrap();
    let agent = create_agent(&state, &access, "Dashboard").await;
    let agent_handle = frona::core::Handle::try_new(agent["handle"].as_str().unwrap()).unwrap();
    let dir = state
        .storage_service
        .agent_workspace_path(&user.handle, &agent_handle)
        .join("dist");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("index.html"), "dashboard fixture")
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let app = App {
        id: "matrix-app".into(),
        agent_id: agent["id"].as_str().unwrap().into(),
        user_id: user.id.clone(),
        handle: frona::handle!("dashboard"),
        name: "Dashboard".into(),
        description: None,
        kind: "static".into(),
        command: None,
        static_dir: Some("dist".into()),
        port: None,
        status: AppStatus::Serving,
        pid: None,
        manifest: serde_json::json!({}),
        chat_id: "unused".into(),
        crash_fix_attempts: 0,
        last_accessed_at: None,
        created_at: now,
        updated_at: now,
    };
    SurrealRepo::<App>::new(state.db.clone())
        .create(&app)
        .await
        .unwrap();
    upload_test_file(&state, &access, "readme.txt", b"private fixture").await;
    (state, tmp, user)
}

async fn issue(state: &AppState, user: &User, kind: TokenType) -> CreatedToken {
    state
        .token_service
        .create_token(
            &state.keypair_service,
            user,
            CreateTokenRequest {
                token_type: kind,
                principal: Principal::user(&user.id),
                ttl_secs: 300,
                name: "matrix".into(),
                scopes: vec![],
                refresh_pair_id: Some(frona::core::repository::new_id()),
                extensions: Some(serde_json::json!({"chat_id": "voice-fixture"})),
            },
        )
        .await
        .unwrap()
}

async fn resign(state: &AppState, token: &str, token_type: Option<&str>, expired: bool) -> String {
    let jwt = JwtService::new();
    let kid = jwt.decode_unverified_header(token).unwrap().kid.unwrap();
    let verification_key = state.keypair_service.get_verifying_key(&kid).await.unwrap();
    let mut claims: frona::auth::models::Claims = jwt.verify(token, &verification_key).unwrap();
    if let Some(kind) = token_type {
        claims.token_type = kind.into();
    }
    if expired {
        claims.exp = (chrono::Utc::now() - chrono::Duration::hours(1)).timestamp() as usize;
    }
    let (key, kid) = state
        .keypair_service
        .get_signing_key(&format!("user:{}", claims.sub))
        .await
        .unwrap();
    jwt.sign(&claims, &key, &kid).unwrap()
}

#[tokio::test]
async fn token_type_entrypoint_matrix() {
    let (state, _tmp, user) = fixture().await;
    for kind in TOKEN_TYPES {
        for gate in GATES {
            let token = issue(&state, &user, kind).await;
            let response = gate.request(&state, &token.jwt).await;
            gate.assert_response(&response, gate.accepts(kind), kind.as_str());
        }
    }
}

#[tokio::test]
async fn expired_revoked_and_unknown_tokens_fail_at_every_gate() {
    let (state, _tmp, user) = fixture().await;
    for kind in TOKEN_TYPES {
        for gate in GATES {
            let token = issue(&state, &user, kind).await;
            let expired = resign(&state, &token.jwt, None, true).await;
            gate.assert_response(
                &gate.request(&state, &expired).await,
                false,
                &format!("expired {kind}"),
            );
            if !kind.is_stateless() {
                state
                    .token_service
                    .repo()
                    .delete(&token.token_id)
                    .await
                    .unwrap();
                gate.assert_response(
                    &gate.request(&state, &token.jwt).await,
                    false,
                    &format!("revoked {kind}"),
                );
            }
        }
    }
    // Correctly signed, live DB record: only its unrecognized type makes it invalid.
    let token = issue(&state, &user, TokenType::Access).await;
    let unknown = resign(&state, &token.jwt, Some("future-type"), false).await;
    for gate in GATES {
        gate.assert_response(&gate.request(&state, &unknown).await, false, "unknown type");
    }
}

#[tokio::test]
async fn app_bootstrap_cookie_and_bearer_fallback_still_work() {
    let (state, _tmp, user) = fixture().await;
    let (_, refresh) = state
        .token_service
        .create_session_pair(&state.keypair_service, &user)
        .await
        .unwrap();
    let response = Gate::AppBootstrap.request(&state, &refresh).await;
    Gate::AppBootstrap.assert_response(&response, true, "legitimate refresh cookie");
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    let app_token = frona::api::cookie::extract_app_session_from_cookie_header(cookie).unwrap();
    let claims = state
        .token_service
        .validate_access(&state.keypair_service, app_token)
        .await
        .unwrap();
    assert_eq!(claims.sub, user.id);
    let response = Gate::AppCookie.request(&state, app_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"dashboard fixture");

    // A rejected Bearer credential can still fall back to a valid app cookie.
    let response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .uri("/apps/dashboard/")
                .header("authorization", format!("Bearer {refresh}"))
                .header("cookie", format!("app_session={app_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Bootstrap validates but must not consume the refresh pair.
    let response = Gate::Refresh.request(&state, &refresh).await;
    Gate::Refresh.assert_response(&response, true, "refresh after bootstrap");
    let json = body_json(response).await;
    state
        .token_service
        .validate_api(&state.keypair_service, json["token"].as_str().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn ephemeral_and_pat_principals_and_scopes_are_preserved() {
    let (state, _tmp, user) = fixture().await;
    for kind in [TokenType::Ephemeral, TokenType::Pat] {
        for scopes in [vec![], vec!["memory".to_string()]] {
            let token = state
                .token_service
                .create_token(
                    &state.keypair_service,
                    &user,
                    CreateTokenRequest {
                        token_type: kind,
                        principal: Principal::user(&user.id),
                        ttl_secs: 300,
                        name: "scoped".into(),
                        scopes: scopes.clone(),
                        refresh_pair_id: None,
                        extensions: Some(serde_json::json!({"example": "preserved"})),
                    },
                )
                .await
                .unwrap();
            let claims = state
                .token_service
                .validate_api(&state.keypair_service, &token.jwt)
                .await
                .unwrap();
            assert_eq!(
                claims.scopes,
                if scopes.is_empty() {
                    None
                } else {
                    Some(scopes.clone())
                }
            );
            assert_eq!(
                claims.extensions,
                Some(serde_json::json!({"example": "preserved"}))
            );
            if kind.is_stateless() {
                assert!(
                    state
                        .token_service
                        .repo()
                        .find_active_by_id(&token.token_id)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            let response = routes::memory_pkm::router()
                .with_state(state.clone())
                .oneshot(auth_get("/api/memory/pkm/sync/config", &token.jwt))
                .await
                .unwrap();
            // With scope, authentication succeeds and the disabled PKM backend gives 404.
            assert_eq!(
                response.status(),
                if scopes.is_empty() {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::NOT_FOUND
                }
            );
        }
        let agent = state
            .agent_service
            .find_by_handle(&user.id, "dashboard")
            .await
            .unwrap()
            .unwrap();
        let token = state
            .token_service
            .create_token(
                &state.keypair_service,
                &user,
                CreateTokenRequest {
                    token_type: kind,
                    principal: Principal::agent(&agent.id),
                    ttl_secs: 300,
                    name: "agent".into(),
                    scopes: vec![],
                    refresh_pair_id: None,
                    extensions: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            Gate::PrincipalApi
                .request(&state, &token.jwt)
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            Gate::UserApi.request(&state, &token.jwt).await.status(),
            StatusCode::FORBIDDEN
        );
    }
}
