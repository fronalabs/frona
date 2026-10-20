//! Deterministic transport failures; successful connections use the real Browserless test.
mod helpers;

use axum::{
    body::Body,
    extract::{Request, State},
    response::Response,
    routing::get,
};
use base64::Engine;
use frona::{
    core::config::BrowserConfig,
    tool::{
        AgentTool,
        browser::{session::BrowserSessionManager, tool::BrowserTool},
    },
};
use sha1::Digest;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
};

#[derive(Clone, Copy)]
enum Failure {
    Close,
    Malformed,
    Unauthorized,
}

struct FailureServer {
    manager: Arc<BrowserSessionManager>,
    attempts: Arc<AtomicUsize>,
    closed: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FailureServer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FailureServer {
    async fn new(failure: Failure) -> Self {
        #[derive(Clone)]
        struct Fault {
            failure: Failure,
            attempts: Arc<AtomicUsize>,
            closed: Arc<Notify>,
        }
        async fn fail(State(fault): State<Fault>, mut request: Request) -> Response {
            fault.attempts.fetch_add(1, Ordering::SeqCst);
            if matches!(fault.failure, Failure::Unauthorized) {
                return Response::builder().status(401).body(Body::empty()).unwrap();
            }
            let key = request.headers()["sec-websocket-key"].as_bytes();
            let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(
                [key, b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"].concat(),
            ));
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                let mut socket = hyper_util::rt::TokioIo::new(upgrade.await.unwrap());
                // A proper close frame or reserved opcode; no successful CDP implementation.
                let frame = if matches!(fault.failure, Failure::Close) {
                    [0x88, 0]
                } else {
                    [0x83, 0]
                };
                socket.write_all(&frame).await.unwrap();
                fault.closed.notify_one();
                let mut remaining = Vec::new();
                let _ = socket.read_to_end(&mut remaining).await;
            });
            Response::builder()
                .status(101)
                .header("Connection", "Upgrade")
                .header("Upgrade", "websocket")
                .header("Sec-WebSocket-Accept", accept)
                .body(Body::empty())
                .unwrap()
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(Notify::new());
        let router = axum::Router::new()
            .route("/", get(fail))
            .route(
                "/sessions",
                get(|| async { axum::Json(serde_json::json!([])) }),
            )
            .with_state(Fault {
                failure,
                attempts: attempts.clone(),
                closed: closed.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager = Arc::new(BrowserSessionManager::new(Some(BrowserConfig {
            ws_url: format!("ws://{}", listener.local_addr().unwrap()),
            api_token: None,
            profiles_path: "/profiles".into(),
            connection_timeout_ms: 1000,
        })));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            manager,
            attempts,
            closed,
            server,
        }
    }
}

#[tokio::test]
async fn disconnect_retries_are_bounded_but_auth_and_protocol_failures_are_not_retried() {
    for (failure, count, message) in [
        (Failure::Close, 3, "clos"),
        (Failure::Unauthorized, 1, "401"),
        (Failure::Malformed, 1, "opcode"),
    ] {
        let fixture = FailureServer::new(failure).await;
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            fixture
                .manager
                .connection(&frona::handle!("alice"), "failure"),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(fixture.attempts.load(Ordering::SeqCst), count, "{error}");
        assert!(
            error.to_string().to_lowercase().contains(message),
            "{error}"
        );
    }
}

#[tokio::test]
async fn cancelling_browser_tool_stops_connection_retries() {
    let fixture = FailureServer::new(Failure::Close).await;
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let app = helpers::app_state::build(&db).await;
    let ctx = helpers::mock_context();
    let cancel = ctx.cancel_token.clone();
    let tool = BrowserTool::new(fixture.manager.clone(), app.state.vault_service.clone());
    let request = tokio::spawn(async move {
        tool.execute(
            "browser_navigate",
            serde_json::json!({"url":"about:blank"}),
            &ctx,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), fixture.closed.notified())
        .await
        .unwrap();
    tokio::time::pause();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    let error = request.await.unwrap().err().unwrap();
    assert!(error.to_string().contains("cancelled"));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(fixture.attempts.load(Ordering::SeqCst), 1);
}
