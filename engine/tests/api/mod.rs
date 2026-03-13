mod agents;
mod auth;
mod chats;
mod contacts;
mod files;
mod messages;
mod misc;
mod navigation;
mod security;
mod spaces;
mod tasks;
mod vaults;

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use frona::agent::service::AgentService;
use frona::db::repo::generic::SurrealRepo;
use frona::storage::StorageService;
use frona::db::init as db;
use frona::api::routes;
use frona::core::config::Config;
use frona::core::metrics::setup_metrics_recorder;
use frona::core::state::AppState;
use surrealdb::engine::local::Mem;
use surrealdb::Surreal;
use tower::ServiceExt;

async fn test_app_state() -> (AppState, tempfile::TempDir) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
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
    let storage = StorageService::new(&config);
    let agent_service = AgentService::new(
        SurrealRepo::new(db.clone()),
        &config.cache,
        std::path::PathBuf::from(&config.storage.shared_config_dir).join("agents"),
    );
    let metrics = setup_metrics_recorder();
    let state = AppState::new(db, &config, None, agent_service, storage, metrics);
    (state, tmp)
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .merge(routes::auth::router())
        .merge(routes::agents::router())
        .merge(routes::chats::router())
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
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\n{path}\r\n"
        )
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
    assert_eq!(resp.status(), StatusCode::OK, "upload_test_file({filename}) failed");
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
                "username": username,
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
