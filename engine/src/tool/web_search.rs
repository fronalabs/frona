use std::env;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use serde::Deserialize;
use serde_json::Value;

use crate::error::AppError;
use crate::tool::{AgentTool, ToolContext, ToolDefinition, ToolOutput};

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

// --- Tavily Provider ---

pub struct TavilyProvider {
    client: reqwest::Client,
    api_key: String,
}

impl TavilyProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
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
            return Err(AppError::Http { status: status.as_u16(), message: text });
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

// --- Brave Provider ---

pub struct BraveProvider {
    client: reqwest::Client,
    api_key: String,
}

impl BraveProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
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
            return Err(AppError::Http { status: status.as_u16(), message: text });
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

// --- SearXNG Provider ---

pub struct SearxngProvider {
    client: reqwest::Client,
    base_url: String,
}

impl SearxngProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
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
            .query(&[
                ("q", query),
                ("format", "json"),
                ("pageno", "1"),
            ])
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("SearXNG request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Http { status: status.as_u16(), message: text });
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

// --- WebSearchTool ---

pub struct WebSearchTool {
    provider: Option<Arc<dyn SearchProvider>>,
}

impl WebSearchTool {
    pub fn new(provider: Option<Arc<dyn SearchProvider>>) -> Self {
        Self { provider }
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

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web and return structured results with titles, URLs, and snippets.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5, max: 20)"
                    }
                },
                "required": ["query"]
            }),
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let provider = self.provider.as_ref().ok_or_else(|| {
            AppError::Tool(
                "No search provider configured. Set one of the following environment variables:\n\
                 - TAVILY_API_KEY (for Tavily search)\n\
                 - BRAVE_API_KEY (for Brave search)\n\
                 - SEARXNG_BASE_URL (for SearXNG, e.g. http://localhost:3400)\n\
                 You can also set SEARCH_PROVIDER explicitly to: tavily, brave, or searxng"
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

// --- Factory ---

pub fn create_search_provider() -> Option<Arc<dyn SearchProvider>> {
    if let Ok(provider_name) = env::var("SEARCH_PROVIDER") {
        return match provider_name.to_lowercase().as_str() {
            "tavily" => {
                let api_key = env::var("TAVILY_API_KEY").ok()?;
                Some(Arc::new(TavilyProvider::new(api_key)))
            }
            "brave" => {
                let api_key = env::var("BRAVE_API_KEY").ok()?;
                Some(Arc::new(BraveProvider::new(api_key)))
            }
            "searxng" => {
                let base_url = env::var("SEARXNG_BASE_URL").ok()?;
                Some(Arc::new(SearxngProvider::new(base_url)))
            }
            other => {
                tracing::warn!(provider = %other, "Unknown search provider");
                None
            }
        };
    }

    if let Ok(api_key) = env::var("TAVILY_API_KEY") {
        return Some(Arc::new(TavilyProvider::new(api_key)));
    }
    if let Ok(api_key) = env::var("BRAVE_API_KEY") {
        return Some(Arc::new(BraveProvider::new(api_key)));
    }
    if let Ok(base_url) = env::var("SEARXNG_BASE_URL") {
        return Some(Arc::new(SearxngProvider::new(base_url)));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockProvider {
        results: Vec<SearchResult>,
    }

    #[async_trait]
    impl SearchProvider for MockProvider {
        async fn search(&self, _query: &str, max_results: usize) -> Result<Vec<SearchResult>, AppError> {
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

    fn mock_context() -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        ToolContext {
            user: crate::models::user::User {
                id: "u".into(), email: "e".into(), name: "n".into(),
                password_hash: String::new(),
                created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
            },
            agent: crate::agent::models::Agent {
                id: "a".into(), user_id: None, name: "a".into(),
                description: String::new(), model_group: "p".into(), enabled: true,
                tools: vec![], sandbox_config: None, max_concurrent_tasks: None,
                avatar: None, identity: Default::default(),
                heartbeat_interval: None, next_heartbeat_at: None,
                heartbeat_chat_id: None,
                created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
            },
            chat: crate::chat::models::Chat {
                id: "c".into(), user_id: "u".into(), space_id: None,
                task_id: None, agent_id: "a".into(), title: None,
                archived_at: None,
                created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
            },
            event_tx: tx,
        }
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
        let tool = WebSearchTool::new(Some(provider));
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
        let tool = WebSearchTool::new(Some(provider));
        let ctx = mock_context();

        let args = serde_json::json!({ "query": "rust", "max_results": 1 });
        let output = tool.execute("web_search", args, &ctx).await.unwrap();
        let text = output.text_content();
        assert!(text.contains("Rust Programming"));
        assert!(!text.contains("Rust Book"));
    }

    #[tokio::test]
    async fn test_web_search_tool_missing_query() {
        let provider = Arc::new(MockProvider {
            results: vec![],
        });
        let tool = WebSearchTool::new(Some(provider));
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
        let tool = WebSearchTool::new(Some(provider));
        let ctx = mock_context();

        let args = serde_json::json!({ "query": "rust", "max_results": 100 });
        let output = tool.execute("web_search", args, &ctx).await.unwrap();
        assert!(output.text_content().contains("Rust Programming"));
    }

    #[tokio::test]
    async fn test_web_search_tool_no_provider() {
        let tool = WebSearchTool::new(None);
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
