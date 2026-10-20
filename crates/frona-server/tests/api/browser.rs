use super::*;
use frona::core::config::BrowserConfig;
use frona::credential::vault::models::CreateLocalItemRequest;
use frona::tool::browser::session::BrowserSessionManager;
use std::sync::Arc;
use wiremock::matchers::{path, query_param, query_param_is_missing};

#[tokio::test]
async fn debugger_auth_preserves_the_ui_and_never_forwards_upstream_diagnostics() {
    let (mut state, _directory) = test_app_state_with_sandbox(true).await;
    let (user_token, user_id) =
        register_user(&state, "browseruser", "browser@example.com", "password123").await;
    let (other_token, _) = register_user(
        &state,
        "otherbrowser",
        "otherbrowser@example.com",
        "password123",
    )
    .await;
    let credential = state
        .vault_service
        .create_credential(
            &user_id,
            CreateLocalItemRequest::BrowserProfile {
                name: "Browser".into(),
            },
        )
        .await
        .unwrap();
    let route = format!("/api/browser/debugger/{}", credential.id);
    let upstream = MockServer::start().await;
    const SECRET: &str = "browser-private +/&?#=é";
    const HTML: &str = "<!doctype html><title>Existing debugger UI</title>";
    let logs = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_env_filter("frona::api::routes::browser=debug,frona::api::error=error")
        .with_writer(logs.reopen().unwrap())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    for configured in [Some(SECRET), None] {
        state.browser_session_manager = Arc::new(BrowserSessionManager::new(Some(BrowserConfig {
            ws_url: format!(
                "{}/prefix/?region=a%26b&--user-data-dir=stale",
                upstream.uri().replacen("http://", "ws://", 1)
            ),
            api_token: configured.map(str::to_owned),
            profiles_path: "/profiles +&#".into(),
            ..Default::default()
        })));
        for status in [200, 302, 401, 500] {
            let mut mock = Mock::given(method("GET"))
                .and(path("/prefix/debugger/"))
                .and(query_param("region", "a&b"))
                .and(query_param(
                    "--user-data-dir",
                    "/profiles +&#/browseruser/browser",
                ));
            mock = match configured {
                Some(token) => mock.and(query_param("token", token)),
                None => mock.and(query_param_is_missing("token")),
            };
            let response = if status == 200 {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(HTML)
            } else {
                ResponseTemplate::new(status)
                    .insert_header("location", "/debugger/?token=browser-private")
                    .insert_header("x-error-detail", "browser-private")
                    .set_body_string(SECRET)
            };
            mock.respond_with(response).expect(1).mount(&upstream).await;
            let response = build_app(state.clone())
                .oneshot(auth_get(&route, &user_token))
                .await
                .unwrap();
            let expected = if status == 200 {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            assert_eq!(response.status(), expected);
            assert!(!response.headers().contains_key("location"));
            assert!(!response.headers().contains_key("x-error-detail"));
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            if status == 200 {
                assert_eq!(text, HTML);
            }
            assert!(!text.contains("browser-private"));
            let text = std::fs::read_to_string(logs.path()).unwrap();
            assert!(!text.contains("browser-private"));
            if status != 200 {
                assert!(
                    text.contains("Browserless debugger returned HTTP"),
                    "{text}"
                );
            }
            upstream.verify().await;
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0]
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key == "--user-data-dir")
                    .count(),
                1
            );
            upstream.reset().await;
        }
        let denied = build_app(state.clone())
            .oneshot(auth_get(&route, &other_token))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }
}

// A real HTTP peer lets the tests observe cancellation at the upstream socket.
async fn accept_debugger_request(listener: &tokio::net::TcpListener) -> tokio::net::TcpStream {
    use tokio::io::AsyncReadExt;
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        assert!(request.len() < 16 * 1024);
        assert_ne!(socket.read_buf(&mut request).await.unwrap(), 0);
    }
    assert!(request.starts_with(b"GET /debugger/?"));
    socket
}

#[tokio::test]
async fn debugger_streams_incrementally_and_releases_timed_out_or_disconnected_upstreams() {
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    let (mut state, _directory) = test_app_state_with_sandbox(true).await;
    let (token, user_id) =
        register_user(&state, "streamuser", "stream@example.com", "password123").await;
    let credential = state
        .vault_service
        .create_credential(
            &user_id,
            CreateLocalItemRequest::BrowserProfile {
                name: "Stream browser".into(),
            },
        )
        .await
        .unwrap();
    let route = format!("/api/browser/debugger/{}", credential.id);
    let listener = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
    state.browser_session_manager = Arc::new(BrowserSessionManager::new(Some(BrowserConfig {
        ws_url: format!("ws://{}", listener.local_addr().unwrap()),
        connection_timeout_ms: 1000,
        ..Default::default()
    })));

    // Missing response headers time out, and dropping the request closes its socket.
    let upstream = listener.clone();
    let peer = tokio::spawn(async move {
        let mut socket = accept_debugger_request(&upstream).await;
        assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0);
    });
    let response = timeout(
        Duration::from_secs(10),
        build_app(state.clone()).oneshot(auth_get(&route, &token)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        body_json(response).await["error"],
        "Browserless debugger response timed out"
    );
    timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();

    // Once headers arrive, a stalled requested body frame ends the stream after 30s.
    let upstream = listener.clone();
    let peer = tokio::spawn(async move {
        let mut socket = accept_debugger_request(&upstream).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nfirst")
            .await
            .unwrap();
        assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0);
    });
    let response = timeout(
        Duration::from_secs(10),
        build_app(state.clone()).oneshot(auth_get(&route, &token)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "first"
    );
    tokio::time::pause();
    let next = body.frame();
    tokio::pin!(next);
    assert!(matches!(
        futures::poll!(&mut next),
        std::task::Poll::Pending
    ));
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = next.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("designated timeout"), "{error}");
    tokio::time::resume();
    drop(body);
    timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();

    // The first bytes must arrive before the producer is allowed to finish a large body.
    const CHUNK_SIZE: usize = 64 * 1024;
    const CHUNKS: usize = 128;
    let upstream = listener.clone();
    let (continue_body, permitted) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut socket = accept_debugger_request(&upstream).await;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nCache-Control: no-store\r\n\r\nfirst", 5 + CHUNK_SIZE * CHUNKS).as_bytes()).await.unwrap();
        permitted.await.unwrap();
        let chunk = vec![b'x'; CHUNK_SIZE];
        for _ in 0..CHUNKS {
            socket.write_all(&chunk).await.unwrap();
        }
    });
    let response = timeout(
        Duration::from_secs(10),
        build_app(state.clone()).oneshot(auth_get(&route, &token)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/html");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.headers()["content-length"],
        (5 + CHUNK_SIZE * CHUNKS).to_string()
    );
    let mut body = response.into_body();
    assert_eq!(
        timeout(Duration::from_secs(5), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        "first"
    );
    continue_body.send(()).unwrap();
    let received = timeout(Duration::from_secs(10), async {
        let mut received = 0;
        while let Some(frame) = body.frame().await {
            let bytes = frame.unwrap().into_data().unwrap();
            assert!(bytes.iter().all(|byte| *byte == b'x'));
            received += bytes.len();
        }
        received
    })
    .await
    .unwrap();
    assert_eq!(received, CHUNK_SIZE * CHUNKS);
    peer.await.unwrap();

    // A real downstream client disconnect must close the still-stalled upstream.
    let upstream = listener.clone();
    let peer = tokio::spawn(async move {
        let mut socket = accept_debugger_request(&upstream).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nfirst")
            .await
            .unwrap();
        assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0);
    });
    let frontend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{}", frontend.local_addr().unwrap(), route);
    let server = tokio::spawn(async move {
        axum::serve(frontend, build_app(state)).await.unwrap();
    });
    let mut response = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.chunk().await.unwrap().unwrap(), "first");
    drop(response);
    timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    server.abort();
}
