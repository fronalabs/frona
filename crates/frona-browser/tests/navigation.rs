//! Navigation regressions using real Chrome and a deterministic HTTP fixture.
//! Run through build/test-browser.sh (isolated Browserless with host aliases).
use frona_browser::{BrowserConnection, Error};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

struct Site {
    port: u16,
    task: JoinHandle<()>,
}
impl Drop for Site {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Site {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let mut requests = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.spawn(async move {
                    let mut buf = [0; 8192];
                    let n = socket.read(&mut buf).await.unwrap();
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/");
                    if path == "/stalled" { std::future::pending::<()>().await; }
                    if path == "/slow" { tokio::time::sleep(Duration::from_millis(600)).await; }
                    if path == "/late" { tokio::time::sleep(Duration::from_millis(2500)).await; }
                    if path == "/brief" { tokio::time::sleep(Duration::from_millis(200)).await; }
                    if path == "/redirect-away" || path == "/redirect-back" {
                        let destination = if path == "/redirect-away" { format!("http://external.test:{port}/stalled") } else { format!("http://ui.frona.test:{port}/slow") };
                        let response = format!("HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        socket.write_all(response.as_bytes()).await.unwrap();
                        return;
                    }
                    if path == "/redirect" {
                        socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /ready\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        return;
                    }
                    let body = match path {
                        "/lazy" => format!("<iframe loading='lazy' style='margin-top:10000px' src='http://ui.frona.test:{port}/stalled'></iframe>"),
                        "/third-party" => format!("<iframe src='http://external.test:{port}/stalled'></iframe>"),
                        "/same-domain" => format!("<iframe src='http://ui.frona.test:{port}/slow'></iframe>"),
                        "/frame-timeout" => format!("<iframe src='http://ui.frona.test:{port}/stalled'></iframe>"),
                        "/frame-redirect-away" => format!("<iframe src='http://ui.frona.test:{port}/redirect-away'></iframe>"),
                        "/frame-redirect-back" => format!("<iframe src='http://external.test:{port}/redirect-back'></iframe><script src='/brief'></script>"),
                        "/late-frame" => format!("<script>onmessage=()=>document.body.id='frame-finished'</script><iframe src='http://ui.frona.test:{port}/late'></iframe>"),
                        "/late" => "<script>parent.postMessage('ready','*')</script>".into(),
                        "/failed-frame" => "<iframe src='http://ui.frona.test:1/'></iframe>".into(),
                        "/detached-frame" => format!("<iframe src='http://ui.frona.test:{port}/stalled'></iframe><script>setTimeout(()=>document.querySelector('iframe').remove(),100)</script>"),
                        "/srcdoc-frame" => format!("<iframe srcdoc=\"<script src='http://host.frona.test:{port}/slow'></script>\"></iframe>"),
                        "/parser-stalled" => "<script src='/stalled'></script>".into(),
                        _ => String::new(),
                    };
                    let body = format!("<!doctype html><title>Ready</title><h1>Ready</h1>{body}");
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                });
                while requests.try_join_next().is_some() {}
            }
        });
        Self { port, task }
    }
    fn url(&self, path: &str) -> String {
        format!("http://host.frona.test:{}{path}", self.port)
    }
}
async fn connect() -> BrowserConnection {
    let endpoint = std::env::var("FRONA_TEST_BROWSER_WS_URL").expect("run build/test-browser.sh");
    BrowserConnection::connect(&endpoint, Duration::from_secs(2), Duration::from_secs(120))
        .await
        .unwrap()
}
#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn lazy_and_third_party_frames_do_not_hold_up_ready_dom() {
    let site = Site::start().await;
    let conn = connect().await;
    for path in [
        "/lazy",
        "/third-party",
        "/frame-redirect-away",
        "/detached-frame",
    ] {
        let result =
            tokio::time::timeout(Duration::from_secs(1), conn.navigate(&site.url(path), true))
                .await;
        assert!(matches!(result, Ok(Ok(_))), "{path}: {result:?}");
    }
}
#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn waits_for_started_sibling_domain_frames() {
    let site = Site::start().await;
    let conn = connect().await;
    let start = Instant::now();
    conn.navigate(&site.url("/same-domain"), true)
        .await
        .unwrap();
    assert!(start.elapsed() >= Duration::from_millis(600));
    let text = conn
        .evaluate(
            "document.querySelector('iframe').contentWindow.length",
            false,
        )
        .await;
    // The elapsed assertion verifies the frame response was awaited even across origins.
    assert!(text.is_ok());
}
#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn no_wait_skips_loading_and_stalled_main_document_fails() {
    let site = Site::start().await;
    let conn = connect().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        conn.navigate(&site.url("/parser-stalled"), false),
    )
    .await
    .unwrap()
    .unwrap();
    for path in ["/parser-stalled", "/stalled"] {
        let result = conn.navigate(&site.url(path), true).await;
        assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    }
    tokio::time::timeout(
        Duration::from_secs(1),
        conn.navigate(&site.url("/ready"), true),
    )
    .await
    .unwrap()
    .unwrap();
}
#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn redirects_repeated_navigation_and_errors() {
    let site = Site::start().await;
    let conn = connect().await;
    for path in ["/redirect", "/ready", "/ready#fragment", "/ready"] {
        let info = conn.navigate(&site.url(path), true).await.unwrap();
        assert!(info.url.contains("/ready"), "{info:?}");
        assert_eq!(info.title, "Ready");
    }
    assert!(conn.navigate("http://[invalid", true).await.is_err());
    assert!(
        conn.navigate("http://host.frona.test:1/", true)
            .await
            .is_err()
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        conn.navigate(&site.url("/ready"), true),
    )
    .await
    .unwrap()
    .unwrap();
}
#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn iframe_timeout_returns_warning_and_keeps_page_usable() {
    let site = Site::start().await;
    let conn = connect().await;
    let info = conn
        .navigate(&site.url("/frame-timeout"), true)
        .await
        .unwrap();
    assert_eq!(info.title, "Ready");
    assert_eq!(info.warnings.len(), 1, "{info:?}");
    assert!(info.warnings[0].contains("ui.frona.test"));
    assert!(
        conn.evaluate("document.body.innerText", false)
            .await
            .unwrap()
            .as_str()
            .unwrap()
            .contains("Ready")
    );
    let next = conn.navigate(&site.url("/ready"), true).await.unwrap();
    assert!(next.warnings.is_empty());
}

#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn frame_redirects_and_inherited_origins_use_the_same_domain_policy() {
    let site = Site::start().await;
    let conn = connect().await;
    for path in ["/frame-redirect-back", "/srcdoc-frame"] {
        let start = Instant::now();
        let info = conn.navigate(&site.url(path), true).await.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(600),
            "{path}: {info:?}"
        );
        assert!(info.warnings.is_empty(), "{path}: {info:?}");
    }
}
#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn failed_iframe_is_a_warning_and_cancelled_navigation_can_be_retried() {
    let site = Site::start().await;
    let conn = connect().await;
    let info = conn
        .navigate(&site.url("/failed-frame"), true)
        .await
        .unwrap();
    assert_eq!(info.warnings.len(), 1, "{info:?}");
    assert!(info.warnings[0].contains("ERR_UNSAFE_PORT"), "{info:?}");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            conn.navigate(&site.url("/stalled"), true)
        )
        .await
        .is_err()
    );
    assert!(
        conn.navigate(&site.url("/ready"), true)
            .await
            .unwrap()
            .warnings
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires isolated Browserless"]
async fn warning_does_not_cancel_a_frame_that_can_still_finish() {
    let site = Site::start().await;
    let conn = connect().await;
    let info = conn.navigate(&site.url("/late-frame"), true).await.unwrap();
    assert_eq!(info.warnings.len(), 1, "{info:?}");
    conn.wait_for_selector("#frame-finished", Duration::from_secs(2))
        .await
        .unwrap();
}
