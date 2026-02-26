use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::sync::RwLock;

use crate::core::error::AppError;

use crate::core::config::BrowserConfig;

#[derive(serde::Deserialize)]
struct BrowserlessSession {
    #[serde(rename = "browserId")]
    browser_id: String,
    #[serde(rename = "type")]
    session_type: Option<String>,
    #[serde(rename = "userDataDir", default)]
    user_data_dir: Option<String>,
}

struct ManagedSession {
    session: browser_use::BrowserSession,
}

#[derive(Clone)]
pub struct BrowserSessionManager {
    config: Option<BrowserConfig>,
    sessions: Arc<RwLock<HashMap<String, ManagedSession>>>,
}

impl BrowserSessionManager {
    pub fn new(config: Option<BrowserConfig>) -> Self {
        Self {
            config,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn config(&self) -> Option<&BrowserConfig> {
        self.config.as_ref()
    }

    fn profile_key(user_id: &str, provider: &str) -> String {
        format!("{user_id}/{provider}")
    }

    pub async fn get_or_create_session(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<(), AppError> {
        let key = Self::profile_key(user_id, provider);

        {
            let sessions = self.sessions.read().await;
            if sessions.contains_key(&key) {
                return Ok(());
            }
        }

        let config = self.config.as_ref().ok_or_else(|| {
            AppError::Browser("Browser is not configured (FRONA_BROWSER_WS_URL not set)".into())
        })?;

        self.kill_browserless_sessions_for_profile(user_id, provider)
            .await;

        let ws_url = config.ws_url_for_profile(user_id, provider);
        tracing::debug!(ws_url = %ws_url, browserless_ws_url = %config.ws_url, "Connecting to browser");

        let options = browser_use::ConnectionOptions::new(&ws_url)
            .timeout(config.connection_timeout_ms);

        let session = browser_use::BrowserSession::connect(options)
            .map_err(|e| AppError::Browser(format!("Failed to connect to browser: {e}")))?;

        let mut sessions = self.sessions.write().await;
        sessions.insert(key, ManagedSession { session });

        Ok(())
    }

    async fn list_browserless_sessions(&self) -> Vec<BrowserlessSession> {
        let Some(config) = self.config.as_ref() else { return vec![] };
        let http_base = config.http_base_url();
        let client = Client::builder(TokioExecutor::new()).build_http::<Body>();

        let sessions_url = format!("{http_base}/sessions");
        let req = match hyper::Request::get(&sessions_url).body(Body::empty()) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to build sessions list request: {e}");
                return vec![];
            }
        };

        let resp = match client.request(req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to list browserless sessions: {e}");
                return vec![];
            }
        };

        let body = match resp.into_body().collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => {
                tracing::warn!("Failed to read sessions response: {e}");
                return vec![];
            }
        };

        match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to parse sessions response: {e}");
                vec![]
            }
        }
    }

    async fn kill_browserless_session_ids(&self, browser_ids: &[String]) {
        if browser_ids.is_empty() {
            return;
        }
        let Some(config) = self.config.as_ref() else { return };
        let http_base = config.http_base_url();
        let client = Client::builder(TokioExecutor::new()).build_http::<Body>();

        for id in browser_ids {
            let kill_url = format!("{http_base}/kill/{id}");
            let req = match hyper::Request::get(&kill_url).body(Body::empty()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Failed to build kill request: {e}");
                    continue;
                }
            };
            if let Err(e) = client.request(req).await {
                tracing::warn!(browser_id = %id, "Failed to kill session: {e}");
            }
        }
        tracing::info!(count = browser_ids.len(), "Killed browserless sessions");
    }

    async fn kill_browserless_sessions_for_profile(&self, user_id: &str, provider: &str) {
        let Some(config) = self.config.as_ref() else { return };
        let profile_path = config.profile_path(user_id, provider);
        let profile_str = profile_path.to_string_lossy();

        let sessions = self.list_browserless_sessions().await;
        let ids: Vec<String> = sessions
            .into_iter()
            .filter(|s| {
                s.session_type.as_deref() == Some("browser")
                    && s.user_data_dir
                        .as_deref()
                        .is_some_and(|d| profile_str.ends_with(d) || d.ends_with(profile_str.as_ref()))
            })
            .map(|s| s.browser_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        self.kill_browserless_session_ids(&ids).await;
    }

    pub async fn kill_all_sessions(&self) {
        let sessions = self.list_browserless_sessions().await;
        let ids: Vec<String> = sessions
            .into_iter()
            .filter(|s| s.session_type.as_deref() == Some("browser"))
            .map(|s| s.browser_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        self.kill_browserless_session_ids(&ids).await;
    }

    async fn execute_on_session(
        &self,
        key: &str,
        tool_name: &str,
        params: &serde_json::Value,
    ) -> Result<String, AppError> {
        let sessions = self.sessions.read().await;
        let managed = sessions
            .get(key)
            .ok_or_else(|| AppError::Browser("Session not found after creation".into()))?;

        let result = managed
            .session
            .execute_tool(tool_name, params.clone())
            .map_err(|e| AppError::Browser(format!("Tool execution failed: {e}")))?;

        if !result.success {
            Ok(format!(
                "Error: {}",
                result.error.unwrap_or_else(|| "Unknown error".to_string())
            ))
        } else {
            Ok(result
                .data
                .map(|v| v.to_string())
                .unwrap_or_default())
        }
    }

    pub async fn execute_tool(
        &self,
        user_id: &str,
        provider: &str,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<String, AppError> {
        let key = Self::profile_key(user_id, provider);

        self.get_or_create_session(user_id, provider).await?;

        match self.execute_on_session(&key, tool_name, &params).await {
            Ok(result) => Ok(result),
            Err(e) if e.to_string().contains("connection is closed") => {
                tracing::warn!("Browser session dead, reconnecting");
                self.sessions.write().await.remove(&key);
                self.get_or_create_session(user_id, provider).await?;
                self.execute_on_session(&key, tool_name, &params).await
            }
            Err(e) if e.to_string().contains("No active tab found") => {
                tracing::warn!("No active tab found, reconnecting browser session");
                self.sessions.write().await.remove(&key);
                self.get_or_create_session(user_id, provider).await?;

                match self.execute_on_session(&key, tool_name, &params).await {
                    Ok(result) => Ok(result),
                    Err(e) if e.to_string().contains("No active tab found")
                        && tool_name == "navigate" =>
                    {
                        tracing::warn!(
                            "Still no active tab after reconnect, creating new tab"
                        );
                        self.execute_on_session(&key, "new_tab", &params).await
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    pub async fn close_session(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<(), AppError> {
        let key = Self::profile_key(user_id, provider);
        let mut sessions = self.sessions.write().await;
        if let Some(managed) = sessions.remove(&key) {
            managed
                .session
                .close()
                .map_err(|e| AppError::Browser(format!("Failed to close session: {e}")))?;
        }
        Ok(())
    }
}
