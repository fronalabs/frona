use super::{
    catalog::{ModelCatalogSnapshot, ModelCatalogStore},
    parameters::{self, ParameterCatalogSnapshot},
};
use crate::CatalogError;
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::Mutex;

pub const MODELS_URL: &str = "https://models.dev/catalog.json";
pub const PARAMETERS_URL: &str = "https://modelparams.dev/api/v1/models.json";
pub const FRESH_FOR: i64 = 86_400;
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const USER_AGENT: &str = concat!(
    "Frona/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/fronalabs/frona)"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Models,
    Parameters,
}

impl Source {
    pub fn url(self) -> &'static str {
        match self {
            Self::Models => MODELS_URL,
            Self::Parameters => PARAMETERS_URL,
        }
    }

    fn filename(self) -> &'static str {
        match self {
            Self::Models => "models_dev_snapshot.json",
            Self::Parameters => "modelparams_dev_snapshot.json",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SourceDocument {
    pub source: String,
    pub fetched_at: DateTime<Utc>,
    pub digest: String,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    pub raw: String,
}

fn read_document(source: Source, directory: &Path) -> Result<SourceDocument, CatalogError> {
    let path = directory.join(source.filename());
    if std::fs::metadata(&path).map_err(invalid)?.len() > MAX_SOURCE_BYTES * 2 {
        return Err(invalid("catalog file too large"));
    }
    let document =
        serde_json::from_str(&std::fs::read_to_string(path).map_err(invalid)?).map_err(invalid)?;
    validate(source, &document)?;
    Ok(document)
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

enum Parsed {
    Models(ModelCatalogSnapshot),
    Parameters(ParameterCatalogSnapshot),
}

fn validate(source: Source, document: &SourceDocument) -> Result<Parsed, CatalogError> {
    if document.source != source.url()
        || document.raw.len() as u64 > MAX_SOURCE_BYTES
        || digest(document.raw.as_bytes()) != document.digest
    {
        return Err(invalid("source identity, size, or digest mismatch"));
    }
    match source {
        Source::Models => {
            let mut parsed = super::loader::parse(&document.raw)?;
            if parsed.providers.is_empty() || parsed.entries.is_empty() {
                return Err(invalid("empty models catalog"));
            }
            parsed.fetched_at = document.fetched_at;
            Ok(Parsed::Models(parsed))
        }
        Source::Parameters => Ok(Parsed::Parameters(parameters::parse(&document.raw)?)),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceStatus {
    pub source: String,
    pub origin: Option<&'static str>,
    pub state: &'static str,
    pub version: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

struct State {
    origin: &'static str,
    document: Option<SourceDocument>,
    error: Option<String>,
}

struct CatalogSource {
    kind: Source,
    state: RwLock<State>,
    refresh: Mutex<()>,
}

#[derive(Clone)]
pub struct CatalogSources {
    pub models: ModelCatalogStore,
    pub parameters: Arc<ArcSwap<ParameterCatalogSnapshot>>,
    models_source: Arc<CatalogSource>,
    parameters_source: Arc<CatalogSource>,
    cache_dir: PathBuf,
    http: reqwest::Client,
}

impl CatalogSources {
    /// Load persisted data only. Missing sources are downloaded by startup.
    pub fn load(cache_dir: &Path) -> Self {
        Self::load_with_bundled(cache_dir, None)
    }

    /// Image catalogs are ordinary files, never embedded in the executable.
    /// Each source chooses the newer valid cache or image document independently.
    pub fn load_with_bundled(cache_dir: &Path, bundled_dir: Option<&Path>) -> Self {
        let mut models = ModelCatalogSnapshot::empty();
        let mut parameters = ParameterCatalogSnapshot::empty();
        let mut load = |kind: Source| {
            let seed = bundled_dir
                .filter(|dir| dir.join(kind.filename()).exists())
                .map(|dir| read_document(kind, dir));
            let mut error = seed
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .map(ToString::to_string);
            let mut selected = seed.and_then(Result::ok);
            let mut origin = "bundled";
            let path = cache_dir.join(kind.filename());
            match std::fs::metadata(&path).and_then(|meta| {
                if meta.len() > MAX_SOURCE_BYTES * 2 {
                    return Err(std::io::Error::other("catalog cache too large"));
                }
                std::fs::read_to_string(&path)
            }) {
                Ok(raw) => match serde_json::from_str::<SourceDocument>(&raw)
                    .map_err(invalid)
                    .and_then(|doc| {
                        validate(kind, &doc)?;
                        Ok(doc)
                    }) {
                    Ok(cache)
                        if selected
                            .as_ref()
                            .is_none_or(|seed| cache.fetched_at >= seed.fetched_at) =>
                    {
                        selected = Some(cache);
                        origin = "cache";
                    }
                    Ok(_) => {}
                    Err(err) => error = Some(err.to_string()),
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => error = Some(err.to_string()),
            }
            if let Some(doc) = &selected {
                match validate(kind, doc).expect("selected source was validated") {
                    Parsed::Models(value) => models = value,
                    Parsed::Parameters(value) => parameters = value,
                }
            }
            Arc::new(CatalogSource {
                kind,
                state: RwLock::new(State {
                    origin,
                    document: selected,
                    error,
                }),
                refresh: Mutex::new(()),
            })
        };
        let models_source = load(Source::Models);
        let parameters_source = load(Source::Parameters);
        Self {
            models: ModelCatalogStore::new(models),
            parameters: Arc::new(ArcSwap::from_pointee(parameters)),
            models_source,
            parameters_source,
            cache_dir: cache_dir.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent(USER_AGENT)
                .build()
                .expect("catalog HTTP client"),
        }
    }

    fn source(&self, kind: Source) -> &CatalogSource {
        match kind {
            Source::Models => &self.models_source,
            Source::Parameters => &self.parameters_source,
        }
    }

    pub fn status(&self, kind: Source, now: DateTime<Utc>) -> SourceStatus {
        let state = self.source(kind).state.read().expect("catalog state");
        SourceStatus {
            source: kind.url().into(),
            origin: state.document.as_ref().map(|_| state.origin),
            state: match &state.document {
                None => "unavailable",
                Some(doc) if (now - doc.fetched_at).num_seconds() < FRESH_FOR => "fresh",
                Some(_) => "stale",
            },
            version: state.document.as_ref().map(|doc| doc.digest.clone()),
            fetched_at: state.document.as_ref().map(|doc| doc.fetched_at),
            last_error: state.error.clone(),
        }
    }

    pub async fn refresh_due(&self, now: DateTime<Utc>) {
        let refresh = |kind| async move {
            if self.status(kind, now).state != "fresh"
                && let Err(error) = self
                    .refresh_from(kind, kind.url(), now, Duration::from_secs(10))
                    .await
            {
                tracing::warn!(source=kind.url(),%error,"Catalog refresh failed; keeping last valid snapshot");
            }
        };
        tokio::join!(refresh(Source::Models), refresh(Source::Parameters));
    }

    /// First-run development bootstrap. Existing valid data never waits on HTTP.
    pub async fn download_missing(&self) {
        self.download_missing_from(MODELS_URL, PARAMETERS_URL).await;
    }

    /// Build-time policy: both sources must download and pass full validation.
    /// Uses the same fetch, schema checks and atomic persistence as runtime refresh.
    pub async fn download_all(&self) -> Result<(), CatalogError> {
        self.download_all_from(MODELS_URL, PARAMETERS_URL).await
    }

    async fn download_all_from(
        &self,
        models_url: &str,
        parameters_url: &str,
    ) -> Result<(), CatalogError> {
        let now = Utc::now();
        let (models, parameters) = tokio::join!(
            self.refresh_from(Source::Models, models_url, now, Duration::from_secs(30)),
            self.refresh_from(
                Source::Parameters,
                parameters_url,
                now,
                Duration::from_secs(30)
            )
        );
        models?;
        parameters?;
        Ok(())
    }

    async fn download_missing_from(&self, models_url: &str, parameters_url: &str) {
        let download = |kind, url| async move {
            let now = Utc::now();
            if self.status(kind, now).state == "unavailable"
                && let Err(error) = self
                    .refresh_from(kind, url, now, Duration::from_secs(10))
                    .await
            {
                tracing::warn!(source=kind.url(),%error,"Initial catalog download failed; continuing without source metadata");
            }
        };
        tokio::join!(
            download(Source::Models, models_url),
            download(Source::Parameters, parameters_url)
        );
    }

    pub async fn refresh_from(
        &self,
        kind: Source,
        url: &str,
        now: DateTime<Utc>,
        timeout: Duration,
    ) -> Result<(), CatalogError> {
        let source = self.source(kind);
        let _guard = source.refresh.lock().await;
        let previous = source.state.read().expect("catalog state").document.clone();
        let result = self
            .fetch_and_persist(source, url, now, timeout, previous)
            .await;
        if let Err(error) = &result {
            source.state.write().expect("catalog state").error = Some(error.to_string());
        }
        result
    }

    async fn fetch_and_persist(
        &self,
        source: &CatalogSource,
        url: &str,
        now: DateTime<Utc>,
        timeout: Duration,
        previous: Option<SourceDocument>,
    ) -> Result<(), CatalogError> {
        let mut request = self.http.get(url).timeout(timeout);
        if let Some(doc) = &previous {
            if let Some(etag) = &doc.etag {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            if let Some(modified) = &doc.last_modified {
                request = request.header(reqwest::header::IF_MODIFIED_SINCE, modified);
            }
        }
        let mut response = request.send().await.map_err(invalid)?;
        let headers = response.headers().clone();
        let mut document = if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            previous.ok_or_else(|| invalid("304 without a previous snapshot"))?
        } else {
            if !response.status().is_success() {
                return Err(invalid(format!("HTTP {}", response.status())));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(invalid)? {
                if bytes.len() + chunk.len() > MAX_SOURCE_BYTES as usize {
                    return Err(invalid("catalog body too large"));
                }
                bytes.extend_from_slice(&chunk);
            }
            let raw = String::from_utf8(bytes).map_err(invalid)?;
            SourceDocument {
                source: source.kind.url().into(),
                fetched_at: now,
                digest: digest(raw.as_bytes()),
                raw,
                etag: None,
                last_modified: None,
            }
        };
        document.fetched_at = now;
        if let Some(value) = headers
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
        {
            document.etag = Some(value.into());
        }
        if let Some(value) = headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
        {
            document.last_modified = Some(value.into());
        }
        let parsed = validate(source.kind, &document)?;
        persist(&self.cache_dir.join(source.kind.filename()), &document)?;
        match parsed {
            Parsed::Models(snapshot) => self.models.swap(snapshot),
            Parsed::Parameters(snapshot) => self.parameters.store(Arc::new(snapshot)),
        }
        *source.state.write().expect("catalog state") = State {
            origin: "remote",
            document: Some(document),
            error: None,
        };
        Ok(())
    }
}

fn persist(path: &Path, document: &SourceDocument) -> Result<(), CatalogError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("cache has no parent"))?;
    std::fs::create_dir_all(parent).map_err(invalid)?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(invalid)?;
    serde_json::to_writer(&mut file, document).map_err(invalid)?;
    file.flush().map_err(invalid)?;
    file.as_file().sync_all().map_err(invalid)?;
    file.persist(path).map_err(invalid)?;
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(invalid)?;
    Ok(())
}

fn invalid(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Internal(format!("catalog source: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };

    fn model_fixture() -> String {
        json!({"providers":{"fixture":{"id":"fixture","name":"Fixture provider","npm":"@ai-sdk/openai","api":"https://provider.example/v1","env":["FIXTURE_KEY"],"models":{"unpriced":{"name":"Unpriced model","attachment":false,"reasoning":true,"reasoning_options":[{"type":"effort","values":[null,"high"]}],"tool_call":true,"open_weights":false,"limit":{"context":8192,"output":1024},"modalities":{"input":["text"],"output":["text"]},"provider":{"npm":"@ai-sdk/openai-compatible","api":"https://route.example/v2"}}}}}}).to_string()
    }

    fn fixture_document(kind: Source) -> SourceDocument {
        let raw = match kind {
            Source::Models => model_fixture(),
            Source::Parameters => json!({"generatedAt":"2026-01-01T00:00:00Z","count":1,"models":[{"provider":"openai","authType":"api_key","apiSurface":"openai-chat-completions","model":"fixture","params":[]}]}).to_string(),
        };
        SourceDocument {
            source: kind.url().into(),
            fetched_at: DateTime::UNIX_EPOCH + chrono::Duration::days(1),
            digest: digest(raw.as_bytes()),
            etag: None,
            last_modified: None,
            raw,
        }
    }

    #[test]
    fn offline_boot_loads_image_files_without_embedded_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let image = tempfile::tempdir().unwrap();
        for kind in [Source::Models, Source::Parameters] {
            persist(&image.path().join(kind.filename()), &fixture_document(kind)).unwrap();
        }
        let sources = CatalogSources::load_with_bundled(dir.path(), Some(image.path()));
        assert_eq!(sources.models.current().providers.len(), 1);
        assert_eq!(sources.models.current().entries.len(), 1);
        assert_eq!(sources.parameters.load().models.len(), 1);
        for kind in [Source::Models, Source::Parameters] {
            let seed = read_document(kind, image.path()).unwrap();
            assert_eq!(
                sources.status(kind, seed.fetched_at).origin,
                Some("bundled")
            );
            assert_eq!(seed.digest, digest(seed.raw.as_bytes()));
            assert_eq!(sources.status(kind, seed.fetched_at).state, "fresh");
            assert_eq!(
                sources
                    .status(kind, seed.fetched_at + chrono::Duration::days(2))
                    .state,
                "stale"
            );
        }
    }

    #[test]
    fn preserves_unpriced_models_reasoning_nulls_names_and_effective_routes() {
        let raw = model_fixture();
        let parsed = super::super::loader::parse(&raw).unwrap();
        assert_eq!(parsed.providers["fixture"].name, "Fixture provider");
        let entry = &parsed.entries["fixture/unpriced"];
        assert!(!entry.has_pricing());
        assert_eq!(entry.name.as_deref(), Some("Unpriced model"));
        assert_eq!(entry.reasoning_options[0]["values"], json!([null, "high"]));
        let route = parsed.effective_route("fixture", "unpriced").unwrap();
        assert_eq!(route.api.as_deref(), Some("https://route.example/v2"));
        assert_eq!(route.npm.as_deref(), Some("@ai-sdk/openai-compatible"));
        assert_eq!(
            parsed.version,
            super::super::loader::parse(&raw).unwrap().version
        );
    }

    #[test]
    fn cache_selection_is_newer_valid_and_independent() {
        let dir = tempfile::tempdir().unwrap();
        let image = tempfile::tempdir().unwrap();
        for kind in [Source::Models, Source::Parameters] {
            persist(&image.path().join(kind.filename()), &fixture_document(kind)).unwrap();
        }
        let mut seed = fixture_document(Source::Models);
        seed.raw = model_fixture();
        seed.digest = digest(seed.raw.as_bytes());
        seed.fetched_at += chrono::Duration::days(1);
        persist(&dir.path().join(Source::Models.filename()), &seed).unwrap();
        std::fs::write(
            dir.path().join(Source::Parameters.filename()),
            "corrupt cache",
        )
        .unwrap();
        let sources = CatalogSources::load_with_bundled(dir.path(), Some(image.path()));
        assert_eq!(sources.models.current().entries.len(), 1);
        assert_eq!(
            sources.status(Source::Models, Utc::now()).origin,
            Some("cache")
        );
        assert_eq!(sources.parameters.load().models.len(), 1);
        assert!(
            sources
                .status(Source::Parameters, Utc::now())
                .last_error
                .is_some()
        );
        seed.fetched_at = DateTime::UNIX_EPOCH;
        persist(&dir.path().join(Source::Models.filename()), &seed).unwrap();
        assert_eq!(
            CatalogSources::load_with_bundled(dir.path(), Some(image.path()))
                .status(Source::Models, Utc::now())
                .origin,
            Some("bundled")
        );
    }

    #[tokio::test]
    async fn conditional_refresh_keeps_data_after_304_and_rejects_invalid_updates() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CatalogSources::load(dir.path());
        let server = MockServer::start().await;
        let now = Utc::now() + chrono::Duration::days(2);
        Mock::given(method("GET"))
            .and(header("user-agent", USER_AGENT))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"fixture\"")
                    .insert_header("last-modified", "Mon, 01 Jun 2026 00:00:00 GMT")
                    .set_body_string(model_fixture()),
            )
            .mount(&server)
            .await;
        sources
            .refresh_from(Source::Models, &server.uri(), now, Duration::from_secs(2))
            .await
            .unwrap();
        let original = sources.status(Source::Models, now).version.unwrap();
        assert_eq!(sources.models.current().entries.len(), 1);
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        sources
            .refresh_from(
                Source::Models,
                &server.uri(),
                now + chrono::Duration::hours(25),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let request = server.received_requests().await.unwrap().pop().unwrap();
        assert_eq!(request.headers.get("if-none-match").unwrap(), "\"fixture\"");
        assert_eq!(
            request.headers.get("if-modified-since").unwrap(),
            "Mon, 01 Jun 2026 00:00:00 GMT"
        );
        assert_eq!(
            sources.status(Source::Models, now).version.as_deref(),
            Some(original.as_str())
        );
        for bad in ["not json", "{}", "{\"providers\":{}}"] {
            server.reset().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(bad))
                .mount(&server)
                .await;
            assert!(
                sources
                    .refresh_from(Source::Models, &server.uri(), now, Duration::from_secs(2))
                    .await
                    .is_err()
            );
            assert_eq!(
                sources.status(Source::Models, now).version.as_deref(),
                Some(original.as_str())
            );
            assert_eq!(
                CatalogSources::load(dir.path())
                    .models
                    .current()
                    .entries
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn one_failed_source_does_not_prevent_parameter_refresh_or_restart() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CatalogSources::load(dir.path());
        let now = Utc::now() + chrono::Duration::days(2);
        let models_before = sources.status(Source::Models, now).version;
        let server = MockServer::start().await;
        let mut raw: serde_json::Value =
            serde_json::from_str(&fixture_document(Source::Parameters).raw).unwrap();
        raw["models"] = json!([raw["models"][0].clone()]);
        raw["count"] = json!(1);
        raw["generatedAt"] = json!(now);
        Mock::given(wiremock::matchers::path("/parameters"))
            .respond_with(ResponseTemplate::new(200).set_body_json(raw))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::path("/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let models_url = format!("{}/models", server.uri());
        let params_url = format!("{}/parameters", server.uri());
        let (models, parameters) = tokio::join!(
            sources.refresh_from(Source::Models, &models_url, now, Duration::from_secs(2)),
            sources.refresh_from(Source::Parameters, &params_url, now, Duration::from_secs(2))
        );
        assert!(models.is_err());
        parameters.unwrap();
        assert_eq!(sources.status(Source::Models, now).version, models_before);
        assert_eq!(sources.parameters.load().models.len(), 1);
        let restarted = CatalogSources::load(dir.path());
        assert_eq!(restarted.parameters.load().models.len(), 1);
        assert_eq!(restarted.status(Source::Models, now).state, "unavailable");
        restarted.parameters_source.state.write().unwrap().document = None;
        assert_eq!(
            restarted.status(Source::Parameters, now).state,
            "unavailable"
        );
    }

    #[tokio::test]
    async fn timeout_schema_and_write_failures_keep_last_valid_source() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [Source::Models, Source::Parameters] {
            persist(&dir.path().join(kind.filename()), &fixture_document(kind)).unwrap();
        }
        let sources = CatalogSources::load(dir.path());
        let server = MockServer::start().await;
        let now = Utc::now();
        let model_version = sources.status(Source::Models, now).version;
        let parameter_version = sources.status(Source::Parameters, now).version;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_string(model_fixture()),
            )
            .mount(&server)
            .await;
        assert!(
            sources
                .refresh_from(Source::Models, &server.uri(), now, Duration::from_millis(5))
                .await
                .is_err()
        );
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"generatedAt":now,"count":1,"models":[{"provider":"openai"}]}),
            ))
            .mount(&server)
            .await;
        assert!(
            sources
                .refresh_from(
                    Source::Parameters,
                    &server.uri(),
                    now,
                    Duration::from_secs(2)
                )
                .await
                .is_err()
        );
        assert_eq!(sources.status(Source::Models, now).version, model_version);
        assert_eq!(
            sources.status(Source::Parameters, now).version,
            parameter_version
        );
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(model_fixture()))
            .mount(&server)
            .await;
        std::fs::remove_file(dir.path().join(Source::Models.filename())).unwrap();
        std::fs::create_dir(dir.path().join(Source::Models.filename())).unwrap();
        assert!(
            sources
                .refresh_from(Source::Models, &server.uri(), now, Duration::from_secs(2))
                .await
                .is_err()
        );
        assert_eq!(sources.status(Source::Models, now).version, model_version);
        assert_eq!(
            sources.status(Source::Parameters, now).version,
            parameter_version
        );
    }

    #[tokio::test]
    async fn development_downloads_missing_sources_to_cache_and_restarts_offline() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CatalogSources::load(dir.path());
        for kind in [Source::Models, Source::Parameters] {
            assert_eq!(sources.status(kind, Utc::now()).state, "unavailable");
        }
        let models = MockServer::start().await;
        let parameters = MockServer::start().await;
        for (kind, server) in [(Source::Models, &models), (Source::Parameters, &parameters)] {
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(fixture_document(kind).raw),
                )
                .expect(1)
                .mount(server)
                .await;
        }
        sources
            .download_missing_from(&models.uri(), &parameters.uri())
            .await;
        let restarted = CatalogSources::load(dir.path());
        // Even stale valid files avoid a startup download; the scheduler refreshes later.
        for kind in [Source::Models, Source::Parameters] {
            assert_eq!(restarted.status(kind, Utc::now()).origin, Some("cache"));
            assert!(read_document(kind, dir.path()).is_ok());
        }
        restarted
            .download_missing_from(&models.uri(), &parameters.uri())
            .await;
        models.verify().await;
        parameters.verify().await;
    }

    #[tokio::test]
    async fn oversized_sources_and_304_without_cache_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CatalogSources::load(dir.path());
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        assert!(
            sources
                .refresh_from(
                    Source::Models,
                    &server.uri(),
                    Utc::now(),
                    Duration::from_secs(2)
                )
                .await
                .is_err()
        );
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
                b' ';
                MAX_SOURCE_BYTES as usize
                    + 1
            ]))
            .mount(&server)
            .await;
        assert!(
            sources
                .refresh_from(
                    Source::Models,
                    &server.uri(),
                    Utc::now(),
                    Duration::from_secs(5)
                )
                .await
                .is_err()
        );
        assert_eq!(
            sources.status(Source::Models, Utc::now()).state,
            "unavailable"
        );
        let path = dir.path().join(Source::Models.filename());
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES * 2 + 1)
            .unwrap();
        assert!(read_document(Source::Models, dir.path()).is_err());
    }

    #[tokio::test]
    async fn strict_download_uses_full_validation_and_loads_offline() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CatalogSources::load(dir.path());
        let server = MockServer::start().await;
        for (kind, route) in [
            (Source::Models, "/models"),
            (Source::Parameters, "/parameters"),
        ] {
            Mock::given(wiremock::matchers::path(route))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(fixture_document(kind).raw),
                )
                .mount(&server)
                .await;
        }
        sources
            .download_all_from(
                &format!("{}/models", server.uri()),
                &format!("{}/parameters", server.uri()),
            )
            .await
            .unwrap();
        let empty = tempfile::tempdir().unwrap();
        let image = CatalogSources::load_with_bundled(empty.path(), Some(dir.path()));
        assert_eq!(image.models.current().entries.len(), 1);
        assert_eq!(image.parameters.load().models.len(), 1);
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        assert!(
            sources
                .download_all_from(&server.uri(), &server.uri())
                .await
                .is_err()
        );
        server.reset().await;
        // Envelope is valid, but the parameter record violates the published schema.
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"generatedAt":"2026-01-01T00:00:00Z", "count":1, "models":[{"provider":"openai"}]}))).mount(&server).await;
        assert!(
            sources
                .download_all_from(&server.uri(), &server.uri())
                .await
                .is_err()
        );
        assert_eq!(
            CatalogSources::load(dir.path())
                .parameters
                .load()
                .models
                .len(),
            1
        );
    }

    #[test]
    fn invalid_identity_digest_and_duplicate_parameters_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [Source::Models, Source::Parameters] {
            let mut doc = fixture_document(kind);
            doc.digest = "invalid".into();
            persist(&dir.path().join(kind.filename()), &doc).unwrap();
            assert!(read_document(kind, dir.path()).is_err());
            doc.digest = digest(doc.raw.as_bytes());
            doc.source = "https://unexpected.invalid".into();
            assert!(validate(kind, &doc).is_err());
        }
        let mut doc = fixture_document(Source::Parameters);
        let mut raw: serde_json::Value = serde_json::from_str(&doc.raw).unwrap();
        raw["models"] = json!([raw["models"][0].clone(), raw["models"][0].clone()]);
        raw["count"] = json!(2);
        doc.raw = raw.to_string();
        doc.digest = digest(doc.raw.as_bytes());
        assert!(validate(Source::Parameters, &doc).is_err());
    }

    #[tokio::test]
    async fn missing_source_failure_does_not_block_other_source_or_startup() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CatalogSources::load(dir.path());
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::path("/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::path("/parameters"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fixture_document(Source::Parameters).raw),
            )
            .mount(&server)
            .await;
        sources
            .download_missing_from(
                &format!("{}/models", server.uri()),
                &format!("{}/parameters", server.uri()),
            )
            .await;
        assert_eq!(
            sources.status(Source::Models, Utc::now()).state,
            "unavailable"
        );
        assert!(
            sources
                .status(Source::Models, Utc::now())
                .last_error
                .is_some()
        );
        assert_eq!(sources.parameters.load().models.len(), 1);
    }
}
