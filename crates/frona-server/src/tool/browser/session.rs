use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::body::Body;
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::sync::{Mutex, RwLock};

use crate::core::config::BrowserConfig;
use crate::core::error::AppError;
use frona_browser::BrowserConnection;

/// Browserless treats `<=0` as `0ms` and 408s the upgrade instantly, so we
/// pass a long positive instead to defeat its 30s per-job default.
const BROWSERLESS_SESSION_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
/// Self-evict before Browserless's hard limit so the next request rebuilds
/// cleanly instead of hitting a forced close mid-op.
const SELF_EVICT_MARGIN: Duration = Duration::from_secs(60);
const CONNECT_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(200)];

#[derive(serde::Deserialize)]
struct BrowserlessSession {
    #[serde(rename = "browserId")]
    browser_id: String,
    #[serde(rename = "type")]
    session_type: Option<String>,
    #[serde(rename = "userDataDir", default)]
    user_data_dir: Option<String>,
}

#[derive(Clone)]
pub struct BrowserSessionManager {
    config: Option<BrowserConfig>,
    sessions: Arc<RwLock<HashMap<String, BrowserConnection>>>,
    profile_locks: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
}

impl BrowserSessionManager {
    pub fn new(config: Option<BrowserConfig>) -> Self {
        Self {
            config,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            profile_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn config(&self) -> Option<&BrowserConfig> {
        self.config.as_ref()
    }

    fn profile_key(user_handle: &crate::core::Handle, provider: &str) -> String {
        format!("{user_handle}/{provider}")
    }

    async fn profile_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.profile_locks.lock().await;
        // Keep only locks still owned by an active connection/close request.
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key.to_string(), Arc::downgrade(&lock));
        lock
    }

    /// Inserts `/` before the query string — browserless v2 returns HTTP 400 without it.
    fn ws_url_for_profile(
        config: &BrowserConfig,
        user_handle: &crate::core::Handle,
        provider: &str,
    ) -> String {
        let user_data_dir = config.profile_path(user_handle, provider);
        let base = config.ws_url.trim_end_matches('/');
        format!(
            "{}/?--user-data-dir={}&timeout={}",
            base,
            user_data_dir.display(),
            BROWSERLESS_SESSION_TIMEOUT.as_millis()
        )
    }

    async fn create_connection(
        &self,
        user_handle: &crate::core::Handle,
        provider: &str,
    ) -> Result<BrowserConnection, AppError> {
        let config = self.config.as_ref().ok_or_else(|| {
            AppError::Browser("Browser is not configured (FRONA_BROWSER_WS_URL not set)".into())
        })?;

        self.kill_browserless_sessions_for_profile(user_handle, provider)
            .await;

        let ws_url = Self::ws_url_for_profile(config, user_handle, provider);
        tracing::debug!(ws_url = %ws_url, browserless_ws_url = %config.ws_url, "Connecting to browser");

        let timeout = Duration::from_millis(config.connection_timeout_ms);
        let lifetime = BROWSERLESS_SESSION_TIMEOUT.saturating_sub(SELF_EVICT_MARGIN);
        let mut retries = CONNECT_RETRY_DELAYS.into_iter();
        loop {
            match BrowserConnection::connect(&ws_url, timeout, lifetime).await {
                Ok(connection) => return Ok(connection),
                Err(error) => {
                    if error.is_retryable_connect_error()
                        && let Some(delay) = retries.next()
                    {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(AppError::Browser(format!(
                        "Failed to connect to browser: {error}"
                    )));
                }
            }
        }
    }

    pub async fn connection(
        &self,
        user_handle: &crate::core::Handle,
        provider: &str,
    ) -> Result<BrowserConnection, AppError> {
        let key = Self::profile_key(user_handle, provider);
        if let Some(connection) = self.sessions.read().await.get(&key).cloned()
            && connection.is_alive()
        {
            return Ok(connection);
        }
        let lock = self.profile_lock(&key).await;
        let _guard = lock.lock().await;
        // Another request may have connected while this one waited.
        if let Some(conn) = self.sessions.read().await.get(&key).cloned() {
            if conn.is_alive() {
                return Ok(conn);
            }
            tracing::warn!(
                user = %user_handle,
                provider = %provider,
                "Cached browser connection is dead; evicting and reconnecting"
            );
        }
        self.sessions.write().await.remove(&key);
        let conn = self.create_connection(user_handle, provider).await?;
        self.sessions.write().await.insert(key, conn.clone());
        Ok(conn)
    }

    fn admin_http_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Body> {
        Client::builder(TokioExecutor::new()).build_http::<Body>()
    }

    async fn list_browserless_sessions(&self) -> Vec<BrowserlessSession> {
        let Some(config) = self.config.as_ref() else {
            return vec![];
        };
        let http_base = config.http_base_url();
        let client = Self::admin_http_client();

        let sessions_url = format!("{http_base}/sessions?token={}", config.api_token());
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
        let Some(config) = self.config.as_ref() else {
            return;
        };
        let http_base = config.http_base_url();
        let client = Self::admin_http_client();

        for id in browser_ids {
            let kill_url = format!("{http_base}/kill/{id}?token={}", config.api_token());
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

    async fn kill_browserless_sessions_for_profile(
        &self,
        user_handle: &crate::core::Handle,
        provider: &str,
    ) {
        let Some(config) = self.config.as_ref() else {
            return;
        };
        let profile_path = config.profile_path(user_handle, provider);
        let profile_str = profile_path.to_string_lossy();

        let sessions = self.list_browserless_sessions().await;
        let ids: Vec<String> = sessions
            .into_iter()
            .filter(|s| {
                s.session_type.as_deref() == Some("browser")
                    && s.user_data_dir.as_deref().is_some_and(|d| {
                        profile_str.ends_with(d) || d.ends_with(profile_str.as_ref())
                    })
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

    pub async fn close_session(
        &self,
        user_handle: &crate::core::Handle,
        provider: &str,
    ) -> Result<(), AppError> {
        let key = Self::profile_key(user_handle, provider);
        let lock = self.profile_lock(&key).await;
        let _guard = lock.lock().await;
        let connection = self.sessions.write().await.remove(&key);
        if let Some(conn) = connection {
            conn.disconnect()
                .await
                .map_err(|e| AppError::Browser(format!("Failed to close session: {e}")))?;
        }
        Ok(())
    }
}

/// No outer timeout: the handler-loop catches transport errors in ms and
/// chromiumoxide's `request_timeout` bounds individual CDP calls.
pub(crate) async fn run_with_reconnect<T, F, Fut>(
    mgr: &BrowserSessionManager,
    ctx: &crate::tool::InferenceContext,
    provider: &str,
    op: F,
) -> Result<T, AppError>
where
    F: Fn(BrowserConnection) -> Fut,
    Fut: std::future::Future<Output = Result<T, frona_browser::Error>>,
{
    let operation = async {
        let conn = mgr.connection(&ctx.user.handle, provider).await?;
        match op(conn.clone()).await {
            Ok(value) => Ok(value),
            Err(error) if error.is_disconnect() => {
                tracing::warn!("Browser session disconnected, reconnecting");
                // Invalidate the failed instance, not a replacement another caller created.
                conn.mark_dead();
                let conn = mgr.connection(&ctx.user.handle, provider).await?;
                op(conn)
                    .await
                    .map_err(|error| AppError::Browser(error.to_string()))
            }
            Err(error) => Err(AppError::Browser(error.to_string())),
        }
    };
    tokio::select! {
        biased;
        _ = ctx.cancel_token.cancelled() => Err(AppError::Tool("Browser operation cancelled".into())),
        result = operation => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(ws_url: &str) -> BrowserConfig {
        BrowserConfig {
            ws_url: ws_url.into(),
            api_token: None,
            profiles_path: "/profiles".into(),
            connection_timeout_ms: 30000,
        }
    }

    #[test]
    fn ws_url_inserts_root_path_before_query_string() {
        let url = BrowserSessionManager::ws_url_for_profile(
            &cfg("ws://browserless:3333"),
            &crate::handle!("alice"),
            "openai",
        );
        assert_eq!(
            url,
            "ws://browserless:3333/?--user-data-dir=/profiles/alice/openai&timeout=86400000"
        );
    }

    #[test]
    fn ws_url_normalises_trailing_slash_on_base() {
        let url = BrowserSessionManager::ws_url_for_profile(
            &cfg("ws://browserless:3333/"),
            &crate::handle!("alice"),
            "openai",
        );
        assert_eq!(
            url,
            "ws://browserless:3333/?--user-data-dir=/profiles/alice/openai&timeout=86400000"
        );
    }

    #[tokio::test]
    async fn historical_profile_locks_are_pruned() {
        let manager = BrowserSessionManager::new(None);
        let active = manager.profile_lock("active").await;
        for i in 0..100 {
            drop(manager.profile_lock(&format!("profile{i}")).await);
            assert!(manager.profile_locks.lock().await.len() <= 2);
        }
        assert!(Arc::ptr_eq(&active, &manager.profile_lock("active").await));
        drop(active);
        drop(manager.profile_lock("last").await);
        assert_eq!(manager.profile_locks.lock().await.len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires FRONA_TEST_BROWSER_WS_URL"]
    async fn real_browser_profiles_share_connections_and_reconnect_independently() {
        let config = cfg(&std::env::var("FRONA_TEST_BROWSER_WS_URL").unwrap());
        let manager = BrowserSessionManager::new(Some(config));
        let user =
            crate::core::Handle::try_new(format!("session-test-{}", std::process::id())).unwrap();
        let requests = (0..8).map(|_| manager.connection(&user, "shared"));
        let connections =
            tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(requests))
                .await
                .unwrap();
        let first = connections
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert!(
            first.iter().all(|connection| connection.is_alive()
                && connection.expires_at() == first[0].expires_at())
        );
        let profile = manager.config().unwrap().profile_path(&user, "shared");
        let sessions = manager.list_browserless_sessions().await;
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.session_type.as_deref() == Some("browser")
                    && session.user_data_dir.as_deref() == profile.to_str())
                .count(),
            1
        );

        // Hold one profile's creation guard while a different profile connects to Chrome.
        let lock = manager
            .profile_lock(&BrowserSessionManager::profile_key(&user, "blocked"))
            .await;
        let guard = lock.lock().await;
        let blocked = manager.connection(&user, "blocked");
        tokio::pin!(blocked);
        assert!(matches!(
            futures::poll!(&mut blocked),
            std::task::Poll::Pending
        ));
        assert!(
            tokio::time::timeout(
                Duration::from_secs(30),
                manager.connection(&user, "independent")
            )
            .await
            .unwrap()
            .unwrap()
            .is_alive()
        );
        drop(guard);
        assert!(
            tokio::time::timeout(Duration::from_secs(30), blocked)
                .await
                .unwrap()
                .unwrap()
                .is_alive()
        );
        drop(lock);

        first[0].mark_dead();
        let replacement =
            tokio::time::timeout(Duration::from_secs(30), manager.connection(&user, "shared"))
                .await
                .unwrap()
                .unwrap();
        assert_ne!(replacement.expires_at(), first[0].expires_at());
        manager.close_session(&user, "shared").await.unwrap();
        assert!(!replacement.is_alive());
        assert!(
            tokio::time::timeout(Duration::from_secs(30), manager.connection(&user, "shared"))
                .await
                .unwrap()
                .unwrap()
                .is_alive()
        );
        for profile in ["shared", "blocked", "independent"] {
            manager.close_session(&user, profile).await.unwrap();
            manager
                .kill_browserless_sessions_for_profile(&user, profile)
                .await;
        }
        assert!(manager.sessions.read().await.is_empty());
        assert!(manager.profile_locks.lock().await.len() <= 1);
    }
}
