use std::env;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use serde::Deserialize;
use serde_json::Value;

use crate::core::error::AppError;
use crate::tool::{InferenceContext, ToolOutput};
use frona_derive::agent_tool;
use frona_text::prompt::PromptLoader;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>, AppError>;
}

pub struct TavilyProvider {
    client: reqwest::Client,
    api_key: String,
}

impl TavilyProvider {
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self { client, api_key }
    }
}

#[derive(Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>, AppError> {
        let body = serde_json::json!({
            "query": query,
            "max_results": max_results,
        });

        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("Tavily request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Http {
                status: status.as_u16(),
                message: text,
            });
        }

        let data: TavilyResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("Tavily parse error: {e}")))?;

        Ok(data
            .results
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.content,
            })
            .collect())
    }
}

pub const KEENABLE_APP_TITLE: &str = "Frona";

/// Keenable web search. Without an API key it uses Keenable's key-free public endpoint.
pub struct KeenableProvider {
    client: reqwest::Client,
    api_key: Option<String>,
}

impl KeenableProvider {
    pub fn new(client: reqwest::Client, api_key: Option<String>) -> Self {
        let api_key = api_key.filter(|k| !k.trim().is_empty());
        Self { client, api_key }
    }

    fn request(&self, query: &str, max_results: usize) -> reqwest::RequestBuilder {
        let body = serde_json::json!({
            "query": query,
            "max_results": max_results,
        });
        let req = match &self.api_key {
            Some(key) => self
                .client
                .post("https://api.keenable.ai/v1/search")
                .header("X-API-Key", key),
            None => self
                .client
                .post("https://api.keenable.ai/v1/search/public")
                .header("X-Keenable-Title", KEENABLE_APP_TITLE),
        };
        req.json(&body)
    }
}

#[derive(Deserialize)]
struct KeenableResponse {
    #[serde(default)]
    results: Vec<KeenableResult>,
}

#[derive(Deserialize)]
struct KeenableResult {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

impl From<KeenableResult> for SearchResult {
    fn from(r: KeenableResult) -> Self {
        let snippet = [r.snippet, r.description]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .unwrap_or_default();
        SearchResult {
            title: r.title,
            url: r.url,
            snippet,
        }
    }
}

#[async_trait]
impl SearchProvider for KeenableProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>, AppError> {
        let resp = self
            .request(query, max_results)
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("Keenable request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Http {
                status: status.as_u16(),
                message: text,
            });
        }

        let data: KeenableResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("Keenable parse error: {e}")))?;

        Ok(data
            .results
            .into_iter()
            .take(max_results)
            .map(SearchResult::from)
            .collect())
    }
}

pub struct BraveProvider {
    client: reqwest::Client,
    api_key: String,
}

impl BraveProvider {
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self { client, api_key }
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWebResults>,
}

#[derive(Deserialize)]
struct BraveWebResults {
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    description: String,
}

#[async_trait]
impl SearchProvider for BraveProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>, AppError> {
        let resp = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &self.api_key)
            .query(&[("q", query), ("count", &max_results.to_string())])
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("Brave request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Http {
                status: status.as_u16(),
                message: text,
            });
        }

        let data: BraveResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("Brave parse error: {e}")))?;

        let results = data
            .web
            .map(|w| w.results)
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.description,
            })
            .collect();

        Ok(results)
    }
}

pub struct SearxngProvider {
    client: reqwest::Client,
    base_url: String,
}

impl SearxngProvider {
    pub fn new(client: reqwest::Client, base_url: String) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

#[derive(Deserialize)]
struct SearxngResponse {
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    title: String,
    url: String,
    content: Option<String>,
}

#[async_trait]
impl SearchProvider for SearxngProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>, AppError> {
        let url = format!("{}/search", self.base_url);

        let resp = self
            .client
            .get(&url)
            .query(&[("q", query), ("format", "json"), ("pageno", "1")])
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("SearXNG request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Http {
                status: status.as_u16(),
                message: text,
            });
        }

        let data: SearxngResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("SearXNG parse error: {e}")))?;

        let results = data
            .results
            .into_iter()
            .take(max_results)
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.content.unwrap_or_default(),
            })
            .collect();

        Ok(results)
    }
}

pub struct WebSearchTool {
    provider: Option<Arc<dyn SearchProvider>>,
    prompts: PromptLoader,
}

impl WebSearchTool {
    pub fn new(provider: Option<Arc<dyn SearchProvider>>, prompts: PromptLoader) -> Self {
        Self { provider, prompts }
    }
}

fn format_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }

    results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[agent_tool]
impl WebSearchTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            AppError::Tool(
                "No search provider configured. Set one of the following environment variables:\n\
                 - TAVILY_API_KEY (for Tavily search)\n\
                 - BRAVE_API_KEY (for Brave search)\n\
                 - KEENABLE_API_KEY (for Keenable search; or set FRONA_SEARCH_PROVIDER=keenable to use it key-free)\n\
                 - FRONA_SEARCH_SEARXNG_BASE_URL (for SearXNG, e.g. http://localhost:3400)\n\
                 You can also set FRONA_SEARCH_PROVIDER explicitly to: tavily, brave, keenable, or searxng"
                    .into(),
            )
        })?;

        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: query".into()))?;

        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| v.min(20) as usize)
            .unwrap_or(5);

        let results = (|| async { provider.search(query, max_results).await })
            .retry(
                ExponentialBuilder::default()
                    .with_max_times(3)
                    .with_min_delay(Duration::from_millis(500))
                    .with_max_delay(Duration::from_secs(30)),
            )
            .sleep(tokio::time::sleep)
            .when(|e| e.is_retryable())
            .notify(|e, dur| {
                tracing::warn!(error = %e, delay = ?dur, "Retrying search request");
            })
            .await?;
        Ok(ToolOutput::text(format_results(&results)))
    }
}

pub fn create_search_provider(
    http: reqwest::Client,
    settings: &crate::core::config::SearchConfig,
) -> Option<Arc<dyn SearchProvider>> {
    if let Some(provider_name) = settings.provider.as_deref() {
        return match provider_name.to_lowercase().as_str() {
            "tavily" => {
                let api_key = env::var("TAVILY_API_KEY").ok()?;
                Some(Arc::new(TavilyProvider::new(http, api_key)))
            }
            "brave" => {
                let api_key = env::var("BRAVE_API_KEY").ok()?;
                Some(Arc::new(BraveProvider::new(http, api_key)))
            }
            "keenable" => {
                let api_key = env::var("KEENABLE_API_KEY").ok();
                Some(Arc::new(KeenableProvider::new(http, api_key)))
            }
            "searxng" => {
                let base_url = settings.searxng_base_url.clone()?;
                Some(Arc::new(SearxngProvider::new(http, base_url)))
            }
            other => {
                tracing::warn!(provider = %other, "Unknown search provider");
                None
            }
        };
    }

    if let Ok(api_key) = env::var("TAVILY_API_KEY") {
        return Some(Arc::new(TavilyProvider::new(http, api_key)));
    }
    if let Ok(api_key) = env::var("BRAVE_API_KEY") {
        return Some(Arc::new(BraveProvider::new(http, api_key)));
    }
    if let Ok(api_key) = env::var("KEENABLE_API_KEY") {
        return Some(Arc::new(KeenableProvider::new(http, Some(api_key))));
    }
    if let Some(base_url) = settings.searxng_base_url.clone() {
        return Some(Arc::new(SearxngProvider::new(http, base_url)));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::AgentTool;

    struct MockProvider {
        results: Vec<SearchResult>,
    }

    #[async_trait]
    impl SearchProvider for MockProvider {
        async fn search(
            &self,
            _query: &str,
            max_results: usize,
        ) -> Result<Vec<SearchResult>, AppError> {
            Ok(self.results.iter().take(max_results).cloned().collect())
        }
    }

    fn sample_results() -> Vec<SearchResult> {
        vec![
            SearchResult {
                title: "Rust Programming".to_string(),
                url: "https://rust-lang.org".to_string(),
                snippet: "A systems programming language".to_string(),
            },
            SearchResult {
                title: "Rust Book".to_string(),
                url: "https://doc.rust-lang.org/book/".to_string(),
                snippet: "The official Rust programming book".to_string(),
            },
        ]
    }

    fn mock_context() -> InferenceContext {
        let broadcast = crate::chat::broadcast::BroadcastService::new();
        let event_sender = broadcast.create_event_sender("u", "c", None);
        InferenceContext::new(
            crate::auth::User {
                id: "u".into(),
                handle: crate::handle!("uu"),
                email: "e".into(),
                name: "n".into(),
                password_hash: String::new(),
                timezone: None,
                groups: Vec::new(),
                deactivated_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            crate::agent::models::Agent {
                id: "a".into(),
                user_id: "test-user".into(),
                handle: crate::handle!("aa"),
                name: "a".into(),
                description: String::new(),
                model_group: "p".into(),
                enabled: true,
                skills: None,
                sandbox_limits: None,
                max_concurrent_tasks: None,
                avatar: None,
                identity: Default::default(),
                prompt: None,
                heartbeat_interval: None,
                next_heartbeat_at: None,
                heartbeat_chat_id: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            crate::chat::models::Chat {
                agent_context_state: Default::default(),
                id: "c".into(),
                user_id: "u".into(),
                space_id: None,
                task_id: None,
                agent_id: "a".into(),
                title: None,
                archived_at: None,
                channel_id: None,
                channel_external_id: None,
                metadata: Default::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            event_sender,
            tokio_util::sync::CancellationToken::new(),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    #[test]
    fn test_format_results_empty() {
        assert_eq!(format_results(&[]), "No results found.");
    }

    #[test]
    fn test_format_results() {
        let results = sample_results();
        let output = format_results(&results);
        assert!(output.contains("1. Rust Programming"));
        assert!(output.contains("https://rust-lang.org"));
        assert!(output.contains("2. Rust Book"));
    }

    #[tokio::test]
    async fn test_web_search_tool_execute() {
        let provider = Arc::new(MockProvider {
            results: sample_results(),
        });
        let tool = WebSearchTool::new(Some(provider), PromptLoader::new("/nonexistent"));
        let ctx = mock_context();

        let args = serde_json::json!({ "query": "rust" });
        let output = tool.execute("web_search", args, &ctx).await.unwrap();
        let text = output.text_content();
        assert!(text.contains("Rust Programming"));
        assert!(text.contains("Rust Book"));
    }

    #[tokio::test]
    async fn test_web_search_tool_max_results() {
        let provider = Arc::new(MockProvider {
            results: sample_results(),
        });
        let tool = WebSearchTool::new(Some(provider), PromptLoader::new("/nonexistent"));
        let ctx = mock_context();

        let args = serde_json::json!({ "query": "rust", "max_results": 1 });
        let output = tool.execute("web_search", args, &ctx).await.unwrap();
        let text = output.text_content();
        assert!(text.contains("Rust Programming"));
        assert!(!text.contains("Rust Book"));
    }

    #[tokio::test]
    async fn test_web_search_tool_missing_query() {
        let provider = Arc::new(MockProvider { results: vec![] });
        let tool = WebSearchTool::new(Some(provider), PromptLoader::new("/nonexistent"));
        let ctx = mock_context();

        let args = serde_json::json!({});
        let result = tool.execute("web_search", args, &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_web_search_tool_max_results_capped() {
        let provider = Arc::new(MockProvider {
            results: sample_results(),
        });
        let tool = WebSearchTool::new(Some(provider), PromptLoader::new("/nonexistent"));
        let ctx = mock_context();

        let args = serde_json::json!({ "query": "rust", "max_results": 100 });
        let output = tool.execute("web_search", args, &ctx).await.unwrap();
        assert!(output.text_content().contains("Rust Programming"));
    }

    #[test]
    fn create_search_provider_returns_none_with_empty_config() {
        let settings = crate::core::config::SearchConfig::default();
        assert!(create_search_provider(crate::build_http_client(), &settings).is_none());
    }

    #[test]
    fn create_search_provider_searxng_from_config() {
        let settings = crate::core::config::SearchConfig {
            provider: Some("searxng".into()),
            searxng_base_url: Some("http://localhost:3400".into()),
        };
        assert!(create_search_provider(crate::build_http_client(), &settings).is_some());
    }

    #[test]
    fn create_search_provider_searxng_autodetect() {
        let settings = crate::core::config::SearchConfig {
            provider: None,
            searxng_base_url: Some("http://localhost:3400".into()),
        };
        assert!(create_search_provider(crate::build_http_client(), &settings).is_some());
    }

    #[test]
    fn create_search_provider_keenable_works_without_a_key() {
        let settings = crate::core::config::SearchConfig {
            provider: Some("keenable".into()),
            searxng_base_url: None,
        };
        assert!(create_search_provider(crate::build_http_client(), &settings).is_some());
    }

    #[test]
    fn keenable_request_uses_public_endpoint_without_key() {
        let provider = KeenableProvider::new(reqwest::Client::new(), Some("  ".into()));
        let req = provider.request("rust", 3).build().unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://api.keenable.ai/v1/search/public"
        );
        assert_eq!(req.headers()["X-Keenable-Title"], KEENABLE_APP_TITLE);
        assert!(req.headers().get("X-API-Key").is_none());
    }

    #[test]
    fn keenable_request_uses_keyed_endpoint_with_key() {
        let provider = KeenableProvider::new(reqwest::Client::new(), Some("k".into()));
        let req = provider.request("rust", 3).build().unwrap();
        assert_eq!(req.url().as_str(), "https://api.keenable.ai/v1/search");
        assert_eq!(req.headers()["X-API-Key"], "k");
    }

    #[test]
    fn keenable_result_falls_back_to_description() {
        let r: KeenableResult = serde_json::from_value(serde_json::json!({
            "title": "T", "url": "https://a", "snippet": "", "description": "D"
        }))
        .unwrap();
        assert_eq!(SearchResult::from(r).snippet, "D");
    }

    #[test]
    fn create_search_provider_unknown_returns_none() {
        let settings = crate::core::config::SearchConfig {
            provider: Some("nonexistent".into()),
            searxng_base_url: None,
        };
        assert!(create_search_provider(crate::build_http_client(), &settings).is_none());
    }

    #[tokio::test]
    async fn test_web_search_tool_no_provider() {
        let tool = WebSearchTool::new(None, PromptLoader::new("/nonexistent"));
        let ctx = mock_context();
        let args = serde_json::json!({ "query": "rust" });
        let result = tool.execute("web_search", args, &ctx).await;
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("No search provider configured"));
                assert!(msg.contains("TAVILY_API_KEY"));
            }
            Ok(_) => panic!("Expected error when no provider configured"),
        }
    }
}
