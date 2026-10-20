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
    let route = format!("/api/browser/debugger/{}/", credential.id);
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
    let mut request = auth_get(&route, &user_token);
    request
        .headers_mut()
        .insert("cookie", "browser_session=expired".parse().unwrap());
    request.headers_mut().remove("authorization");
    let response = build_app(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers()["location"],
        format!("/api/auth/browser/{}", credential.id)
    );
    let canonical = build_app(state.clone())
        .oneshot(auth_get(route.trim_end_matches('/'), &user_token))
        .await
        .unwrap();
    assert_eq!(canonical.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(canonical.headers()["location"], route);

    state.browser_session_manager = Arc::new(BrowserSessionManager::new(Some(BrowserConfig {
        ws_url: upstream.uri().replacen("http://", "ws://", 1),
        connection_timeout_ms: 50,
        ..Default::default()
    })));
    for origin in ["http://evil.example", "https://frona.example"] {
        let mut request = auth_get(&route, &user_token);
        request
            .headers_mut()
            .insert("upgrade", "websocket".parse().unwrap());
        request
            .headers_mut()
            .insert("host", "frona.example".parse().unwrap());
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        assert_eq!(
            build_app(state.clone())
                .oneshot(request)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let requested = Arc::new(tokio::sync::Notify::new());
    let signal = requested.clone();
    Mock::given(method("GET"))
        .and(path("/sessions"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(10))
                .set_body_json(serde_json::json!([]))
        })
        .expect(1)
        .mount(&upstream)
        .await;
    let mut request = auth_get(&route, &user_token);
    request
        .headers_mut()
        .insert("upgrade", "websocket".parse().unwrap());
    let pending = tokio::spawn(async move { build_app(state).oneshot(request).await.unwrap() });
    // Authenticate first: key verification is outside the upstream handshake deadline.
    tokio::time::timeout(std::time::Duration::from_secs(5), requested.notified())
        .await
        .unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    upstream.verify().await;
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
    let route = format!("/api/browser/debugger/{}/", credential.id);
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

#[tokio::test]
#[ignore = "requires FRONA_TEST_BROWSER_WS_URL"]
async fn live_browserless_debugger_ui_and_cdp_are_scoped_to_owner() {
    let config = BrowserConfig {
        ws_url: std::env::var("FRONA_TEST_BROWSER_WS_URL").expect("Browserless URL"),
        api_token: std::env::var("FRONA_TEST_BROWSER_API_TOKEN").ok(),
        profiles_path: "/profiles".into(),
        ..Default::default()
    };
    let (mut state, directory) = test_app_state_with_sandbox(true).await;
    let (token, user_id) = register_user(
        &state,
        "livedebugger",
        "livedebugger@example.com",
        "password123",
    )
    .await;
    let credential = state
        .vault_service
        .create_credential(
            &user_id,
            CreateLocalItemRequest::BrowserProfile {
                name: "Live debugger".into(),
            },
        )
        .await
        .unwrap();
    state.browser_session_manager = Arc::new(BrowserSessionManager::new(Some(config.clone())));
    let manager = state.browser_session_manager.clone();
    let user = frona::core::Handle::try_new("livedebugger").unwrap();
    let connection = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        manager.connection(&user, &credential.provider),
    )
    .await
    .unwrap()
    .unwrap();
    let page = connection
        .navigate(
            "data:text/html,<title>CDP compatibility</title><h1>Browserless</h1>",
            true,
        )
        .await
        .unwrap();
    assert_eq!(page.title, "CDP compatibility");
    let screenshot_path = directory.path().join("browserless.png");
    let screenshot = connection
        .screenshot(&screenshot_path, false)
        .await
        .unwrap();
    let png = std::fs::read(&screenshot_path).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(png.len(), screenshot.size_bytes);
    assert!(png.len() > 100);
    let (foreign_token, foreign_user_id) = register_user(
        &state,
        "foreignbrowser",
        "foreignbrowser@example.com",
        "password123",
    )
    .await;
    let foreign_credential = state
        .vault_service
        .create_credential(
            &foreign_user_id,
            CreateLocalItemRequest::BrowserProfile {
                name: "Foreign browser".into(),
            },
        )
        .await
        .unwrap();
    let foreign_user = frona::core::Handle::try_new("foreignbrowser").unwrap();
    let foreign_connection = manager
        .connection(&foreign_user, &foreign_credential.provider)
        .await
        .unwrap();
    foreign_connection
        .navigate("data:text/html,<title>Foreign private page</title>", true)
        .await
        .unwrap();
    let app = build_app(state);
    let response = app
        .clone()
        .oneshot(auth_get(
            &format!("/api/browser/debugger/{}/", credential.id),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let body = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("<title>Browserless debugger</title>"));
    if let Some(secret) = &config.api_token {
        assert!(!html.contains(secret));
    }

    let scope = format!("/api/browser/debugger/{}/", credential.id);
    let foreign_scope = format!("/api/browser/debugger/{}/", foreign_credential.id);
    let foreign_pages = body_json(
        app.clone()
            .oneshot(auth_get(
                &format!("{foreign_scope}sessions"),
                &foreign_token,
            ))
            .await
            .unwrap(),
    )
    .await;
    let foreign_id = foreign_pages[0]["id"].as_str().unwrap();
    assert_eq!(
        app.clone()
            .oneshot(auth_get(&format!("{foreign_scope}router.js"), &token))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(auth_get(&format!("{scope}kill/all"), &token))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    let frontend = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let host = std::env::var("FRONA_TEST_SERVER_HOST").unwrap_or_else(|_| "host.frona.test".into());
    let base = format!("http://{host}:{}", frontend.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        axum::serve(
            frontend,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let observer = manager.connection(&user, "ui-observer").await.unwrap();
    observer
        .navigate(&format!("{base}/healthz"), true)
        .await
        .unwrap();
    let login = observer
        .evaluate(
            r#"async function () {
        const response = await fetch('/api/auth/login', {
            method: 'POST', credentials: 'include', headers: {'content-type': 'application/json'},
            body: JSON.stringify({identifier:'livedebugger',password:'password123'})
        }); return response.status;
    }"#,
            true,
        )
        .await
        .unwrap();
    assert_eq!(login, 200);
    // Exercise the existing plain link: refresh-cookie gate, scoped cookie, streamed document and real assets.
    observer
        .navigate(
            &format!("{base}/api/browser/debugger/{}", credential.id),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        observer.evaluate("location.pathname", true).await.unwrap(),
        scope
    );
    observer
        .evaluate(
            "document.querySelector('#sessions-button-radio').click()",
            true,
        )
        .await
        .unwrap();
    let links = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let links = observer.evaluate("Array.from(document.querySelectorAll('#sessions-viewer a'), a => ({text:a.textContent,href:a.href}))", true).await.unwrap();
            if links.as_array().is_some_and(|links| !links.is_empty()) { break links; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }).await.unwrap();
    assert_eq!(links.as_array().unwrap().len(), 1);
    assert_eq!(links[0]["text"], "CDP compatibility");
    let inspector = links[0]["href"].as_str().unwrap();
    assert!(inspector.starts_with(&format!("{base}{scope}devtools/inspector.html?")));
    let targets = observer
        .evaluate(
            "async function () { return (await fetch('sessions')).json(); }",
            true,
        )
        .await
        .unwrap();
    // Chrome can retain an initial about:blank page; every returned page must still be in this profile.
    assert!(targets.as_array().unwrap().iter().all(|target| {
        target.as_object().unwrap().len() == 3
            && (target["url"] == "about:blank" || target["title"] == "CDP compatibility")
    }));
    let page_id = targets
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["title"] == "CDP compatibility")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    // Real browser WebSockets carry only the scoped cookie. Prove attachment and foreign-target rejection.
    let ws_probe = |path: &str, method: &str, params: serde_json::Value| {
        format!(
            r#"async function () {{
        const url = new URL({}, location.href); url.protocol = 'ws:';
        return await new Promise(resolve => {{
            const ws = new WebSocket(url); const timer = setTimeout(() => {{ws.close(); resolve('timeout')}}, 10000);
            ws.onopen = () => ws.send(JSON.stringify({{id:1,method:{},params:{}}}));
            ws.onmessage = event => {{ const value = JSON.parse(event.data); if(value.id === 1) {{clearTimeout(timer); ws.close(); resolve(value)}} }};
            ws.onerror = () => {{ clearTimeout(timer); resolve('denied') }};
        }});
    }}"#,
            serde_json::to_string(path).unwrap(),
            serde_json::to_string(method).unwrap(),
            params
        )
    };
    let result = observer
        .evaluate(
            &ws_probe(
                &format!("{scope}devtools/page/{page_id}"),
                "Runtime.evaluate",
                serde_json::json!({"expression":"document.title"}),
            ),
            true,
        )
        .await
        .unwrap();
    assert_eq!(result["result"]["result"]["value"], "CDP compatibility");
    assert_eq!(
        observer
            .evaluate(
                &ws_probe(
                    &format!("{scope}devtools/page/{foreign_id}"),
                    "Runtime.evaluate",
                    serde_json::json!({"expression":"document.title"})
                ),
                true
            )
            .await
            .unwrap(),
        "denied"
    );
    let version = observer
        .evaluate(
            &ws_probe(&scope, "Browser.getVersion", serde_json::json!({})),
            true,
        )
        .await
        .unwrap();
    assert!(
        version["result"]["product"]
            .as_str()
            .unwrap()
            .contains("Chrome")
    );
    observer.navigate(inspector, true).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let attached_title = observer.evaluate(r#"async function () {
                const target = globalThis.SDK?.targetManager?.mainTarget();
                if (!target) return null;
                const result = await target.runtimeAgent().invoke_evaluate({expression:'document.title', returnByValue:true});
                return result.result?.value;
            }"#, true).await.unwrap();
            if attached_title == "CDP compatibility" { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    manager.close_session(&user, "ui-observer").await.unwrap();
    manager
        .close_session(&foreign_user, &foreign_credential.provider)
        .await
        .unwrap();
    server.abort();
    manager
        .close_session(&user, &credential.provider)
        .await
        .unwrap();
}
