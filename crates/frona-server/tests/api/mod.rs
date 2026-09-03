mod activity;
mod admin;
mod agents;
mod app_supervisor;
mod apps;
mod auth;
mod channels;
mod chats;
mod commands;
mod contacts;
mod files;
mod mcp;
mod messages;
mod misc;
mod navigation;
mod notifications;
mod security;
mod spaces;
mod system;
mod tasks;
mod vaults;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode};
use frona::api::middleware::shutdown::shutdown_gate;
use frona::api::routes;
use frona::core::config::Config;
use frona::core::metrics::setup_metrics_recorder;
use frona::core::state::AppState;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::storage::StorageService;
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;
use tower::ServiceExt;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn workspace_resources() -> std::path::PathBuf {
    std::env::current_dir()
        .expect("cwd")
        .ancestors()
        .find(|p| p.join("resources/prompts").exists())
        .expect("workspace resources/ not found from cwd")
        .join("resources")
}

/// Generic mock provider API for adapters whose `on_connect` hits an HTTP API
/// that can be redirected via an `api_url`/base-url config override. Start it,
/// add provider-specific `stub_ok` responses, then set the channel's `api_url`
/// config to `.uri()` so the *real* `on_connect` succeeds against the mock —
/// letting the full connect→route→parse path run with no test-code in prod.
/// Reusable across adapters (Telegram now; WhatsApp Cloud / SMS later).
pub struct MockProviderApi {
    pub server: MockServer,
}

impl MockProviderApi {
    pub async fn start() -> Self {
        Self {
            server: MockServer::start().await,
        }
    }

    pub fn uri(&self) -> String {
        self.server.uri()
    }

    /// Stub every POST whose path matches `path_re` (regex) with a 200 JSON body.
    pub async fn stub_ok(&self, path_re: &str, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path_regex(path_re))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }
}

/// Telegram connect stubs: `getWebhookInfo` (no existing webhook) + `setWebhook`
/// (ok), so `on_connect` reaches Connected against the mock. Point the channel's
/// `api_url` config at `.uri()`.
pub async fn telegram_mock_api() -> MockProviderApi {
    let m = MockProviderApi::start().await;
    m.stub_ok(
        r"(?i)getwebhookinfo",
        serde_json::json!({
            "ok": true,
            "result": {"url": "", "has_custom_certificate": false, "pending_update_count": 0}
        }),
    )
    .await;
    m.stub_ok(
        r"(?i)setwebhook",
        serde_json::json!({"ok": true, "result": true}),
    )
    .await;
    m
}

async fn test_app_state() -> (AppState, tempfile::TempDir) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();
    let resources = workspace_resources();
    let config = Config {
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        storage: frona::core::config::StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: resources.to_string_lossy().into_owned(),
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
    let mut state = AppState::new(
        db.clone(),
        &config,
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics,
        resource_manager,
    );

    // Override the MCP service with a noop package installer so tests don't
    // try to run npx/uvx against fake packages.
    {
        let mcp_repo: std::sync::Arc<dyn frona::tool::mcp::McpServerRepository> =
            std::sync::Arc::new(SurrealRepo::<frona::tool::mcp::McpServer>::new(db));
        let mcp_registry: std::sync::Arc<dyn frona::tool::mcp::McpRegistryClient> =
            std::sync::Arc::new(frona::tool::mcp::PrebuiltMcpRegistryClient::new(
                state.http_client.clone(),
                std::path::PathBuf::from(&base).join("mcp-registry"),
            ));
        let noop: std::sync::Arc<dyn frona::tool::mcp::PackageInstaller> =
            std::sync::Arc::new(frona::tool::mcp::NoopPackageInstaller);
        state.mcp_service = std::sync::Arc::new(frona::tool::mcp::McpServerService::new(
            mcp_repo,
            state.mcp_manager.clone(),
            mcp_registry,
            std::sync::Arc::new(state.vault_service.clone()),
            noop,
            state.tool_manager.clone(),
            state.token_service.clone(),
            state.keypair_service.clone(),
            state.user_service.clone(),
            state.policy_service.clone(),
            state.storage_service.clone(),
            state.config.server.public_base_url(),
            state.config.auth.ephemeral_token_expiry_secs,
        ));
    }
    state.tool_manager.init(&state);
    state.policy_service.sync_base_policies().await.unwrap();
    (state, tmp)
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .merge(routes::admin::router())
        .merge(routes::activity::router())
        .merge(routes::auth::router())
        .merge(routes::agents::router())
        .merge(routes::chats::router())
        .merge(routes::commands::router())
        .merge(routes::channels::router())
        .merge(routes::spaces::router())
        .merge(routes::tasks::router())
        .merge(routes::files::router())
        .merge(routes::contacts::router())
        .merge(routes::navigation::router())
        .merge(routes::messages::router())
        .merge(routes::vaults::router())
        .merge(routes::tools::router())
        .merge(routes::well_known::router())
        .merge(routes::metrics::router())
        .merge(routes::config::router())
        .merge(routes::notifications::router())
        .merge(routes::apps::router())
        .merge(routes::mcp::router())
        .merge(routes::system::router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            shutdown_gate,
        ))
        .with_state(state)
}

fn multipart_upload(token: &str, filename: &str, content: &[u8]) -> Request<Body> {
    let boundary = "----testboundary";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(content);
    bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    Request::builder()
        .method("POST")
        .uri("/api/files")
        .header("authorization", format!("Bearer {token}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(bytes))
        .unwrap()
}

fn multipart_upload_with_path(
    token: &str,
    filename: &str,
    content: &[u8],
    path: &str,
) -> Request<Body> {
    let boundary = "----testboundary";
    let mut bytes = Vec::new();
    // path field
    bytes.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\n{path}\r\n")
            .as_bytes(),
    );
    // file field
    bytes.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(content);
    bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    Request::builder()
        .method("POST")
        .uri("/api/files")
        .header("authorization", format!("Bearer {token}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(bytes))
        .unwrap()
}

async fn upload_test_file(
    state: &AppState,
    token: &str,
    filename: &str,
    content: &[u8],
) -> serde_json::Value {
    let app = build_app(state.clone());
    let req = multipart_upload(token, filename, content);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "upload_test_file({filename}) failed"
    );
    body_json(resp).await
}

fn with_connect_info(req: &mut Request<Body>) {
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
}

async fn body_json(resp: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn register_user(
    state: &AppState,
    username: &str,
    email: &str,
    password: &str,
) -> (String, String) {
    let app = build_app(state.clone());
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "handle": username,
                "email": email,
                "name": username,
                "password": password,
            })
            .to_string(),
        ))
        .unwrap();
    with_connect_info(&mut req);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "register_user({username}) failed"
    );
    let json = body_json(resp).await;
    let token = json["token"].as_str().unwrap().to_string();
    let user_id = json["user"]["id"].as_str().unwrap().to_string();
    (token, user_id)
}

fn auth_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn auth_post_json(uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn auth_put_json(uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn auth_delete(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn auth_patch_json(uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

async fn create_agent(state: &AppState, token: &str, name: &str) -> serde_json::Value {
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/agents",
            token,
            serde_json::json!({
                "name": name,
                "description": "Test agent",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

async fn create_chat(
    state: &AppState,
    token: &str,
    agent_id: &str,
    title: Option<&str>,
) -> serde_json::Value {
    let app = build_app(state.clone());
    let mut body = serde_json::json!({"agent_id": agent_id});
    if let Some(t) = title {
        body["title"] = serde_json::json!(t);
    }
    let resp = app
        .oneshot(auth_post_json("/api/chats", token, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

async fn create_space(state: &AppState, token: &str, name: &str) -> serde_json::Value {
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/spaces",
            token,
            serde_json::json!({"name": name}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

async fn create_task(
    state: &AppState,
    token: &str,
    agent_id: &str,
    title: &str,
) -> serde_json::Value {
    let app = build_app(state.clone());
    let resp = app
        .oneshot(auth_post_json(
            "/api/tasks",
            token,
            serde_json::json!({
                "agent_id": agent_id,
                "title": title,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}
