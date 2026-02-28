use axum::body::Body;
use axum::http::{Request, StatusCode};
use frona::agent::workspace::AgentWorkspaceManager;
use frona::api::db;
use frona::api::routes::voice;
use frona::core::config::Config;
use frona::core::metrics::setup_metrics_recorder;
use frona::core::state::AppState;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;
use tower::ServiceExt;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

async fn test_app_state() -> (AppState, tempfile::TempDir) {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        storage: frona::core::config::StorageConfig {
            workspaces_path: format!("{base}/workspaces"),
            files_path: format!("{base}/files"),
            shared_config_dir: format!("{base}/config"),
        },
        ..Default::default()
    };
    let workspaces = AgentWorkspaceManager::new(&config.storage.workspaces_path, format!("{base}/agents"));
    let metrics = setup_metrics_recorder();
    let state = AppState::new(db, &config, None, workspaces, metrics);
    (state, tmp)
}

#[tokio::test]
async fn twilio_callback_invalid_token_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let app = voice::router().with_state(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/voice/twilio/callback?token=invalid.token.here")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn twilio_ws_invalid_token_returns_403() {
    let (state, _tmp) = test_app_state().await;
    let app = voice::router().with_state(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/voice/twilio/ws?token=bad.token.here")
                .header("upgrade", "websocket")
                .header("connection", "upgrade")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .header("sec-websocket-version", "13")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn twilio_callback_valid_token_returns_xml() {
    use chrono::Utc;
    use frona::api::repo::generic::SurrealRepo;
    use frona::auth::jwt::JwtService;
    use frona::credential::keypair::service::KeyPairService;
    use frona::tool::voice::VoiceCallbackClaims;
    use std::sync::Arc;

    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        storage: frona::core::config::StorageConfig {
            workspaces_path: format!("{base}/workspaces"),
            files_path: format!("{base}/files"),
            shared_config_dir: format!("{base}/config"),
        },
        // voice provider not set — we just test the JWT decode + TwiML generation path
        ..Default::default()
    };

    // Build keypair service with same secret as app state will use
    let kp_svc = KeyPairService::new(
        "test-secret",
        Arc::new(SurrealRepo::new(db.clone())),
    );
    let jwt_svc = JwtService::new();

    let (enc_key, kid) = kp_svc.get_signing_key("voice").await.unwrap();
    let exp = (Utc::now().timestamp() + 300) as usize;
    let claims = VoiceCallbackClaims {
        sub: "user-123".to_string(),
        chat_id: "chat-456".to_string(),
        exp,
        welcome_greeting: None,
        hints: None,
        contact_id: None,
    };
    let token = jwt_svc.sign(&claims, &enc_key, &kid).unwrap();

    let workspaces = AgentWorkspaceManager::new(&config.storage.workspaces_path, format!("{base}/agents"));
    let metrics = setup_metrics_recorder();
    let state = AppState::new(db, &config, None, workspaces, metrics);

    let app = voice::router().with_state(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/voice/twilio/callback?token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.contains("application/xml"), "Expected XML content-type, got: {ct}");

    let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(
        body_str.contains("<ConversationRelay"),
        "Expected ConversationRelay in TwiML:\n{body_str}"
    );
}
