use crate::inference::protocol::parameters::WireParameters;
use axum::body::Bytes;
use rig_core::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

tokio::task_local! { static PARAMETERS: WireParameters; }

pub async fn scope<F: Future>(parameters: WireParameters, future: F) -> F::Output {
    PARAMETERS.scope(parameters, future).await
}

/// SDK transports call this after serialization and before request signing.
pub(crate) fn apply_current(body: &mut serde_json::Value) {
    let _ = PARAMETERS.try_with(|parameters| parameters.apply(body));
}

/// Applies the native JSON merge after Rig has serialized the provider request.
/// Parameters belong to the current async call, never to the cached client.
#[derive(Debug, Clone, Default)]
pub struct WireClient {
    inner: reqwest::Client,
}

impl WireClient {
    pub(crate) fn without_redirects() -> Result<Self, reqwest::Error> {
        Ok(Self {
            inner: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }
}

fn log_transport_error(operation: &str, error: &http_client::Error) {
    let http_client::Error::Instance(error) = error else {
        return;
    };
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error.as_ref());
    let mut causes = Vec::new();
    while let Some(error) = source {
        if causes.len() == 8 {
            break;
        }
        causes.push(transport_cause(error));
        // io::Error::source delegates to the wrapped error's source, skipping
        // the wrapped error itself (which may contain the TLS failure).
        source = error
            .downcast_ref::<std::io::Error>()
            .and_then(|error| error.get_ref().map(|inner| inner as &dyn std::error::Error))
            .or_else(|| error.source());
    }
    tracing::warn!(
        operation,
        ?causes,
        truncated = source.is_some(),
        "Provider HTTP transport failed"
    );
}

fn transport_cause(error: &(dyn std::error::Error + 'static)) -> String {
    // Never format errors themselves: even nested errors can include URL paths,
    // query credentials, DNS names, or arbitrary text supplied by a connector.
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        format!(
            "reqwest(timeout={}, connect={}, dns={})",
            error.is_timeout(),
            error.is_connect(),
            error.is_dns()
        )
    } else if let Some(error) = error.downcast_ref::<hyper_util::client::legacy::Error>() {
        format!("http client(connect={})", error.is_connect())
    } else if let Some(error) = error.downcast_ref::<hyper::Error>() {
        format!(
            "http(timeout={}, incomplete={}, parse={})",
            error.is_timeout(),
            error.is_incomplete_message(),
            error.is_parse()
        )
    } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
        format!("io(kind={:?}, os={:?})", error.kind(), error.raw_os_error())
    } else if let Some(error) = error.downcast_ref::<rustls::Error>() {
        match error {
            rustls::Error::InvalidCertificate(_) => "tls: invalid certificate",
            rustls::Error::NoCertificatesPresented => "tls: missing certificate",
            rustls::Error::InvalidMessage(_) => "tls: invalid message",
            rustls::Error::PeerIncompatible(_) => "tls: incompatible peer",
            rustls::Error::PeerMisbehaved(_) => "tls: misbehaving peer",
            rustls::Error::AlertReceived(_) => "tls: peer alert",
            _ => "tls",
        }
        .into()
    } else {
        "opaque cause".into()
    }
}

fn rewrite(request: Request<Bytes>) -> http_client::Result<Request<Bytes>> {
    let Ok(parameters) = PARAMETERS.try_with(Clone::clone) else {
        return Ok(request);
    };
    if request.method() != http_client::Method::POST {
        return Ok(request);
    }
    let (mut parts, bytes) = request.into_parts();
    let mut body: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| http_client::Error::Instance(Box::new(error)))?;
    if !body.is_object() {
        return Err(http_client::Error::Instance(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "provider request must be a JSON object",
        ))));
    }
    parameters.apply(&mut body);
    let bytes =
        serde_json::to_vec(&body).map_err(|error| http_client::Error::Instance(Box::new(error)))?;
    parts.headers.remove("content-length");
    Ok(Request::from_parts(parts, Bytes::from(bytes)))
}

impl HttpClientExt for WireClient {
    fn send<T, U>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let request = rewrite(request.map(Into::into));
        let inner = self.inner.clone();
        async move {
            inner
                .send(request?)
                .await
                .inspect_err(|error| log_transport_error("send", error))
        }
    }

    fn send_multipart<U>(
        &self,
        request: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        self.inner.send_multipart(request)
    }

    fn send_streaming<T>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let request = rewrite(request.map(Into::into));
        async move {
            self.inner
                .send_streaming(request?)
                .await
                .inspect_err(|error| log_transport_error("send_streaming", error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing::instrument::WithSubscriber;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    const SECRET: &str = "private-provider-key";

    #[derive(Clone, Default)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct CapturedSend {
        result: http_client::Result<()>,
        logs: String,
    }

    async fn capture_send(client: &WireClient, endpoint: &str, streaming: bool) -> CapturedSend {
        let output = LogWriter::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_env_filter("frona::inference::protocol::http=warn")
            .with_writer(move || writer.clone())
            .finish();
        let request = Request::builder()
            .method("POST")
            .uri(format!("{endpoint}/private-{SECRET}?key={SECRET}"))
            .header("authorization", format!("Bearer {SECRET}"))
            .header("x-api-key", SECRET)
            .body(Bytes::from_static(b"{}"))
            .unwrap();
        let result = async {
            if streaming {
                client.send_streaming(request).await.map(|_| ())
            } else {
                client.send::<_, Bytes>(request).await.map(|_| ())
            }
        }
        .with_subscriber(subscriber)
        .await;
        CapturedSend {
            result,
            logs: String::from_utf8(output.0.lock().unwrap().clone()).unwrap(),
        }
    }

    fn client() -> WireClient {
        WireClient {
            inner: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        }
    }

    fn assert_failure_log(logs: &str, streaming: bool) {
        assert_eq!(logs.matches("Provider HTTP transport failed").count(), 1);
        assert!(logs.contains(if streaming {
            "operation=\"send_streaming\""
        } else {
            "operation=\"send\""
        }));
        for sensitive in [
            SECRET,
            "authorization",
            "x-api-key",
            "/private-",
            "key=",
            "://",
        ] {
            assert!(!logs.contains(sensitive));
        }
        assert!(logs.len() < 1024);
    }

    fn request_error(captured: &CapturedSend) -> &reqwest::Error {
        let http_client::Error::Instance(error) = captured.result.as_ref().unwrap_err() else {
            panic!("expected original SDK transport error");
        };
        let error = error.downcast_ref::<reqwest::Error>().unwrap();
        // Logging must not strip the URL or otherwise replace the caller's error.
        assert!(error.url().unwrap().as_str().contains(SECRET));
        error
    }

    #[tokio::test]
    async fn refused_connections_log_underlying_io_cause_and_preserve_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        for streaming in [false, true] {
            let captured = capture_send(&client(), &endpoint, streaming).await;
            assert!(request_error(&captured).is_connect());
            assert_failure_log(&captured.logs, streaming);
            assert!(captured.logs.contains("connect=true"));
            assert!(
                captured
                    .logs
                    .contains("io(kind=ConnectionRefused, os=Some(")
            );
            assert!(captured.logs.contains("truncated=false"));
            assert!(!captured.logs.contains("127.0.0.1"));
        }
    }

    #[tokio::test]
    async fn timeouts_are_identifiable_on_both_send_paths() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
            .mount(&server)
            .await;
        let client = WireClient {
            inner: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_millis(100))
                .build()
                .unwrap(),
        };
        for streaming in [false, true] {
            let captured = capture_send(&client, &server.uri(), streaming).await;
            assert!(request_error(&captured).is_timeout());
            assert_failure_log(&captured.logs, streaming);
            assert!(captured.logs.contains("timeout=true"));
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("{message}")]
    struct ResolverFailure {
        message: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    }

    struct FailingResolver {
        depth: usize,
    }

    impl reqwest::dns::Resolve for FailingResolver {
        fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let mut error: Box<dyn std::error::Error + Send + Sync> = Box::new(
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, SECRET),
            );
            for _ in 0..self.depth {
                error = Box::new(ResolverFailure {
                    message: SECRET.repeat(256),
                    source: error,
                });
            }
            Box::pin(async move { Err(error) })
        }
    }

    #[tokio::test]
    async fn nested_dns_causes_are_useful_bounded_and_never_formatted_verbatim() {
        for depth in [1, 12] {
            let client = WireClient {
                inner: reqwest::Client::builder()
                    .no_proxy()
                    .dns_resolver(Arc::new(FailingResolver { depth }))
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap(),
            };
            for streaming in [false, true] {
                let endpoint = format!("http://{SECRET}.invalid");
                let captured = capture_send(&client, &endpoint, streaming).await;
                assert!(request_error(&captured).is_dns());
                assert!(format!("{:?}", captured.result.as_ref().unwrap_err()).contains(SECRET));
                assert_failure_log(&captured.logs, streaming);
                assert!(captured.logs.contains("dns=true"));
                if depth == 1 {
                    assert!(captured.logs.contains("io(kind=ConnectionRefused"));
                    assert!(captured.logs.contains("truncated=false"));
                } else {
                    assert!(captured.logs.contains("truncated=true"));
                    let causes = captured.logs.split("causes=[").nth(1).unwrap();
                    let causes = causes.split(']').next().unwrap();
                    assert_eq!(causes.split("\", \"").count(), 8);
                }
            }
        }
    }

    #[tokio::test]
    async fn tls_failure_wrapped_in_io_keeps_its_safe_cause() {
        crate::initialize_tls();
        for streaming in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("https://{}", listener.local_addr().unwrap());
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut hello = [0; 4096];
                stream.read(&mut hello).await.unwrap();
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            });
            let captured = capture_send(&client(), &endpoint, streaming).await;
            peer.await.unwrap();
            assert!(request_error(&captured).is_connect());
            assert_failure_log(&captured.logs, streaming);
            assert!(captured.logs.contains("io(kind=InvalidData"));
            assert!(captured.logs.contains("tls: invalid message"));
        }
    }

    #[tokio::test]
    async fn successful_requests_do_not_emit_failure_diagnostics() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(2)
            .mount(&server)
            .await;
        for streaming in [false, true] {
            let captured = capture_send(&client(), &server.uri(), streaming).await;
            assert!(captured.result.is_ok());
            assert!(captured.logs.is_empty());
        }
    }
}
