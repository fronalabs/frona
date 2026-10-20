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
    Arc, Mutex,
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
    requests: Arc<Mutex<Vec<axum::http::Uri>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FailureServer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FailureServer {
    async fn new(failure: Failure) -> Self {
        Self::with_token(failure, None, "").await
    }

    async fn with_token(failure: Failure, token: Option<&str>, query: &str) -> Self {
        #[derive(Clone)]
        struct Fault {
            failure: Failure,
            attempts: Arc<AtomicUsize>,
            closed: Arc<Notify>,
            token: Option<String>,
            admin_token: String,
            requests: Arc<Mutex<Vec<axum::http::Uri>>>,
        }
        fn token_matches(request: &Request, expected: Option<&str>) -> bool {
            let tokens: Vec<_> =
                url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
                    .filter(|(key, _)| key == "token")
                    .map(|(_, value)| value.into_owned())
                    .collect();
            tokens == expected.into_iter().map(str::to_owned).collect::<Vec<_>>()
        }
        async fn admin(State(fault): State<Fault>, request: Request) -> Response {
            fault.requests.lock().unwrap().push(request.uri().clone());
            if !token_matches(&request, Some(&fault.admin_token)) {
                return Response::builder().status(401).body(Body::empty()).unwrap();
            }
            let body = if request.uri().path() == "/sessions" {
                r#"[{"browserId":"id/a?b","type":"browser","userDataDir":"unmatched"}]"#
            } else {
                ""
            };
            Response::new(Body::from(body))
        }
        async fn fail(State(fault): State<Fault>, mut request: Request) -> Response {
            fault.requests.lock().unwrap().push(request.uri().clone());
            fault.attempts.fetch_add(1, Ordering::SeqCst);
            if !token_matches(&request, fault.token.as_deref())
                || matches!(fault.failure, Failure::Unauthorized)
            {
                return Response::builder()
                    .status(401)
                    .header("x-echo", request.uri().to_string())
                    .body(Body::from(request.uri().to_string()))
                    .unwrap();
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = BrowserConfig {
            ws_url: format!("ws://{}{}", listener.local_addr().unwrap(), query),
            api_token: token.map(str::to_owned),
            profiles_path: "/profiles +&#".into(),
            connection_timeout_ms: 1000,
        };
        let expected_token = token.map(str::to_owned).or_else(|| {
            url::form_urlencoded::parse(query.trim_start_matches('?').as_bytes())
                .find(|(key, _)| key == "token")
                .map(|(_, value)| value.into_owned())
        });
        let requests = Arc::new(Mutex::new(Vec::new()));
        let router = axum::Router::new()
            .route("/", get(fail))
            .route("/sessions", get(admin))
            .route("/kill/{id}", get(admin))
            .with_state(Fault {
                failure,
                attempts: attempts.clone(),
                closed: closed.clone(),
                token: expected_token,
                admin_token: token.unwrap_or("frona").into(),
                requests: requests.clone(),
            });
        let manager = Arc::new(BrowserSessionManager::new(Some(config)));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            manager,
            attempts,
            closed,
            requests,
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

#[tokio::test]
async fn session_and_admin_authentication_encode_queries_without_logging_secrets() {
    const SECRET: &str = "browser-secret +/&?#=é";
    // A single subscriber also captures logs from spawned connection handlers.
    let logs = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_env_filter("frona=debug,frona_browser=debug")
        .with_writer(logs.reopen().unwrap())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    for (token, query) in [
        (
            Some(SECRET),
            "?region=a%26b&token=stale&timeout=1&--user-data-dir=old",
        ),
        (None, "?region=a%26b"),
        (None, "?region=a%26b&token=url-secret%2Btoken"),
    ] {
        let fixture = FailureServer::with_token(Failure::Malformed, token, query).await;
        fixture.manager.kill_all_sessions().await;
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            fixture
                .manager
                .connection(&frona::handle!("alice"), "provider +&#"),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        // Only an authenticated WebSocket upgrade can reach the malformed frame.
        assert!(error.to_string().contains("opcode"), "{error}");
        assert_eq!(fixture.attempts.load(Ordering::SeqCst), 1);
        let requests = fixture.requests.lock().unwrap();
        assert!(requests.iter().any(|uri| uri.path() == "/kill/id%2Fa%3Fb"));
        for uri in requests.iter() {
            let pairs: Vec<_> =
                url::form_urlencoded::parse(uri.query().unwrap().as_bytes()).collect();
            assert_eq!(
                pairs.iter().find(|(key, _)| key == "region").unwrap().1,
                "a&b"
            );
            if uri.path() == "/" {
                assert_eq!(pairs.iter().filter(|(key, _)| key == "timeout").count(), 1);
                assert_eq!(
                    pairs.iter().find(|(key, _)| key == "timeout").unwrap().1,
                    "86400000"
                );
                assert_eq!(
                    pairs
                        .iter()
                        .filter(|(key, _)| key == "--user-data-dir")
                        .count(),
                    1
                );
                assert_eq!(
                    pairs
                        .iter()
                        .find(|(key, _)| key == "--user-data-dir")
                        .unwrap()
                        .1,
                    "/profiles +&#/alice/provider +&#"
                );
            }
        }
        let text = std::fs::read_to_string(logs.path()).unwrap();
        assert!(text.contains("Connecting to browser"));
        assert!(!text.contains("browser-secret"));
        assert!(!text.contains("url-secret"));
        assert!(!error.to_string().contains(SECRET));
    }
}
