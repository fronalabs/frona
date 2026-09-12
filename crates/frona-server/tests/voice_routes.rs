use axum::body::Body;
use axum::http::{Request, StatusCode};
use frona::api::routes::voice;
use frona::core::config::Config;
use frona::core::metrics::setup_metrics_recorder;
use frona::core::state::AppState;
use frona::db::init as db;
use frona::storage::StorageService;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};
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
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);
    let resource_manager = std::sync::Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let metrics = setup_metrics_recorder();
    let state = AppState::new(
        db.clone(),
        {
            let mut loaded = frona::core::config::ConfigService::load(
                tempfile::tempdir().unwrap().path().join("config.yaml"),
            )
            .unwrap();
            loaded.config = config.clone();
            frona::core::config::ConfigService::new(loaded).unwrap()
        },
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics,
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );
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
    use frona::auth::User;
    use frona::auth::token::models::TokenType;
    use frona::auth::token::service::CreateTokenRequest;
    use frona::core::Principal;
    use frona::db::repo::generic::SurrealRepo;
    use frona::tool::voice::VoiceCallbackExtensions;

    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let config = Config {
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        storage: frona::core::config::StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        // voice provider not set — we just test the JWT decode + TwiML generation path
        ..Default::default()
    };

    let storage = StorageService::new(&config);
    let resource_manager = std::sync::Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let metrics = setup_metrics_recorder();
    let state = AppState::new(
        db.clone(),
        {
            let mut loaded = frona::core::config::ConfigService::load(
                tempfile::tempdir().unwrap().path().join("config.yaml"),
            )
            .unwrap();
            loaded.config = config.clone();
            frona::core::config::ConfigService::new(loaded).unwrap()
        },
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics,
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );

    // Persist the user so the AppState's token_service can round-trip the token
    // through the ApiToken DB row it creates for access tokens.
    let user = User {
        id: "user-123".to_string(),
        handle: frona::handle!("testuser"),
        email: "test@example.com".to_string(),
        name: "Test".to_string(),
        password_hash: String::new(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let user_repo: SurrealRepo<User> = SurrealRepo::new(db.clone());
    use frona::core::repository::Repository;
    user_repo.create(&user).await.unwrap();

    let extensions = serde_json::to_value(VoiceCallbackExtensions {
        chat_id: "chat-456".to_string(),
        welcome_greeting: None,
        hints: None,
        contact_id: None,
    })
    .unwrap();
    let created = state
        .token_service
        .create_token(
            &state.keypair_service,
            &user,
            CreateTokenRequest {
                token_type: TokenType::Access,
                principal: Principal::agent("receptionist"),
                ttl_secs: 300,
                name: "voice_callback".into(),
                scopes: Vec::new(),
                refresh_pair_id: None,
                extensions: Some(extensions),
            },
        )
        .await
        .unwrap();

    let app = voice::router().with_state(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/voice/twilio/callback?token={}", created.jwt))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.contains("application/xml"),
        "Expected XML content-type, got: {ct}"
    );

    let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(
        body_str.contains("<ConversationRelay"),
        "Expected ConversationRelay in TwiML:\n{body_str}"
    );
}

mod helpers;
