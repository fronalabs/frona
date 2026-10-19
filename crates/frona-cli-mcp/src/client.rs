use frona_api_types::mcp::{
    BridgeCallRequest, BridgeCallResponse, BridgeServerDetail, BridgeServerInfo,
};
use reqwest::{Method, Url};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub struct BridgeClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl BridgeClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("static bridge HTTP client configuration"),
            base_url,
            token,
        }
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<BridgeCallRequest>,
    ) -> Result<T, Error> {
        // Treat server/tool names as individual path segments, never URLs or
        // paths. Dot segments are special even when supplied through Url.
        if segments
            .iter()
            .any(|segment| matches!(*segment, "." | ".."))
        {
            return Err(Error::Invalid("Invalid bridge path segment"));
        }
        let mut url =
            Url::parse(&self.base_url).map_err(|_| Error::Invalid("Invalid bridge base URL"))?;
        url.path_segments_mut()
            .map_err(|_| Error::Invalid("Invalid bridge base URL"))?
            .pop_if_empty()
            .extend(["api", "mcp", "bridge"])
            .extend(segments);
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|_| Error::Invalid("Invalid bridge credentials"))?;
        authorization.set_sensitive(true);
        let mut request = self
            .http
            .request(method, url)
            .header(reqwest::header::AUTHORIZATION, authorization);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(Error::Invalid("Bridge response exceeds the 4 MiB limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                body: serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("error")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned()),
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("Invalid bridge response JSON"))
    }
}

impl crate::BridgeBackend for BridgeClient {
    async fn list_servers(&self) -> Result<Vec<BridgeServerInfo>, Error> {
        self.request(Method::GET, &["servers"], None).await
    }

    async fn server_tools(&self, slug: &str) -> Result<BridgeServerDetail, Error> {
        self.request(Method::GET, &["servers", slug], None).await
    }

    async fn call_tool(
        &self,
        slug: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<BridgeCallResponse, Error> {
        self.request(
            Method::POST,
            &[slug, "call", tool_name],
            Some(BridgeCallRequest { arguments }),
        )
        .await
    }
}

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Api { status: u16, body: String },
    Invalid(&'static str),
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::Api { status, body } => write!(f, "API error ({status}): {body}"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}
