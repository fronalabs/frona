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
        async move { inner.send(request?).await }
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
        async move { self.inner.send_streaming(request?).await }
    }
}
