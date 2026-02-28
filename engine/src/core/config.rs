use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::deserialize_bool_from_anything;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub port: u16,
    pub static_dir: String,
    pub issuer_url: String,
    pub max_concurrent_tasks: usize,
    pub sandbox_disabled: bool,
    pub cors_origins: Option<String>,
    pub base_url: Option<String>,
    pub max_body_size_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 3001,
            static_dir: "frontend/out".into(),
            issuer_url: String::new(),
            max_concurrent_tasks: 10,
            sandbox_disabled: false,
            cors_origins: None,
            base_url: None,
            max_body_size_bytes: 104_857_600,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AuthConfig {
    pub encryption_secret: String,
    pub access_token_expiry_secs: u64,
    pub refresh_token_expiry_secs: u64,
    pub presign_expiry_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            encryption_secret: "dev-secret-change-in-production".into(),
            access_token_expiry_secs: 900,
            refresh_token_expiry_secs: 604800,
            presign_expiry_secs: 86400,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SsoConfig {
    pub enabled: bool,
    pub authority: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: String,
    pub allow_unknown_email_verification: bool,
    pub client_cache_expiration: u64,
    pub only: bool,
    pub signups_match_email: bool,
}

impl Default for SsoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            authority: None,
            client_id: None,
            client_secret: None,
            scopes: "email profile offline_access".into(),
            allow_unknown_email_verification: false,
            client_cache_expiration: 0,
            only: false,
            signups_match_email: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub path: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: "data/db".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub ws_url: String,
    pub profiles_path: String,
    pub connection_timeout_ms: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            ws_url: String::new(),
            profiles_path: "/profiles".into(),
            connection_timeout_ms: 30000,
        }
    }
}

impl BrowserConfig {
    pub fn ws_url_for_profile(&self, username: &str, provider: &str) -> String {
        let user_data_dir = self.profile_path(username, provider);
        format!(
            "{}?--user-data-dir={}",
            self.ws_url,
            user_data_dir.display()
        )
    }

    pub fn http_base_url(&self) -> String {
        self.ws_url
            .replace("ws://", "http://")
            .replace("wss://", "https://")
    }

    pub fn debugger_url_for_credential(&self, credential_id: &str) -> String {
        format!("/api/browser/debugger/{credential_id}")
    }

    pub fn profile_path(&self, username: &str, provider: &str) -> PathBuf {
        PathBuf::from(&self.profiles_path)
            .join(username)
            .join(provider)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SearchConfig {
    pub provider: Option<String>,
    pub searxng_base_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct StorageConfig {
    pub workspaces_path: String,
    pub files_path: String,
    pub shared_config_dir: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            workspaces_path: "data/workspaces".into(),
            files_path: "data/files".into(),
            shared_config_dir: "resources".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SchedulerConfig {
    pub space_compaction_secs: u64,
    pub insight_compaction_secs: u64,
    pub poll_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            space_compaction_secs: 3600,
            insight_compaction_secs: 7200,
            poll_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub backoff_multiplier: f64,
    pub max_backoff_ms: u64,
}

impl RetryConfig {
    pub fn to_backoff(&self) -> backon::ExponentialBuilder {
        backon::ExponentialBuilder::default()
            .with_max_times(self.max_retries as usize)
            .with_min_delay(std::time::Duration::from_millis(self.initial_backoff_ms))
            .with_factor(self.backoff_multiplier as f32)
            .with_max_delay(std::time::Duration::from_millis(self.max_backoff_ms))
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 10,
            initial_backoff_ms: 1_000,
            backoff_multiplier: 2.0,
            max_backoff_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelGroupConfig {
    pub main: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelProviderConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    #[serde(
        default = "serde_aux::prelude::bool_true",
        deserialize_with = "deserialize_bool_from_anything"
    )]
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct InferenceConfig {
    pub max_tool_turns: usize,
    pub default_max_tokens: u64,
    pub compaction_trigger_pct: usize,
    pub history_truncation_pct: usize,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            max_tool_turns: 200,
            default_max_tokens: 8192,
            compaction_trigger_pct: 80,
            history_truncation_pct: 90,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub provider: Option<String>,
    pub twilio_account_sid: Option<String>,
    pub twilio_auth_token: Option<String>,
    pub twilio_from_number: Option<String>,
    pub twilio_voice_id: Option<String>,
    pub twilio_speech_model: Option<String>,
    /// Public-facing base URL used for Twilio callback and WebSocket URLs.
    /// Overrides `server.base_url` for voice only, so cookies remain non-secure
    /// when accessing the app locally while still allowing Twilio to reach a
    /// public ngrok/tunnel endpoint.
    pub callback_base_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub sso: SsoConfig,
    pub database: DatabaseConfig,
    pub browser: Option<BrowserConfig>,
    pub search: SearchConfig,
    pub storage: StorageConfig,
    pub scheduler: SchedulerConfig,
    pub inference: InferenceConfig,
    pub voice: VoiceConfig,
    #[serde(default)]
    pub models: HashMap<String, ModelGroupConfig>,
    #[serde(default)]
    pub providers: HashMap<String, ModelProviderConfig>,
}

pub struct LoadedConfig {
    pub config: Config,
    pub models: Option<crate::inference::config::ModelRegistryConfig>,
}

impl Config {
    pub fn load() -> LoadedConfig {
        let config_path = std::env::var("FRONA_CONFIG")
            .unwrap_or_else(|_| "data/config.yaml".into());

        let yaml_content = std::fs::read_to_string(&config_path).ok();

        let mut builder = config::Config::builder();

        if let Some(ref content) = yaml_content {
            let expanded = expand_env_vars(content);
            builder = builder.add_source(
                config::File::from_str(&expanded, config::FileFormat::Yaml),
            );
        }

        // Collect FRONA_* env vars and remap the key so the section separator
        // becomes "__" while field-name underscores are preserved.
        // e.g. FRONA_BROWSER_WS_URL → browser__ws_url → browser.ws_url
        let frona_env: HashMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("FRONA_") && !matches!(k.as_str(), "FRONA_CONFIG" | "FRONA_LOG_CONFIG" | "FRONA_LOG_LEVEL"))
            .map(|(k, v)| {
                let stripped = k["FRONA_".len()..].to_lowercase();
                let mapped = match stripped.find('_') {
                    Some(pos) => format!("{}__{}", &stripped[..pos], &stripped[pos + 1..]),
                    None => stripped,
                };
                (mapped, v)
            })
            .collect();

        builder = builder.add_source(
            config::Environment::default()
                .source(Some(frona_env))
                .separator("__")
                .try_parsing(true),
        );

        let built = builder.build().expect("Failed to build config");

        let config: Config = built
            .try_deserialize()
            .expect("Failed to deserialize config");

        let models = if !config.models.is_empty() || !config.providers.is_empty() {
            Some(crate::inference::config::ModelRegistryConfig {
                providers: config.providers.clone().into_iter().collect(),
                models: config.models.clone().into_iter().collect(),
            })
        } else {
            None
        };

        if yaml_content.is_some() {
            tracing::info!(path = %config_path, "Loaded config from YAML");
        } else {
            tracing::info!("No config file found, using defaults and env vars");
        }

        if let Ok(mut v) = serde_json::to_value(&config) {
            redact(&mut v, &["auth", "encryption_secret"]);
            redact(&mut v, &["sso", "client_secret"]);
            redact(&mut v, &["voice", "twilio_account_sid"]);
            redact(&mut v, &["voice", "twilio_auth_token"]);
            if let Some(providers) = v.get_mut("providers").and_then(|p| p.as_object_mut()) {
                for provider in providers.values_mut() {
                    redact(provider, &["api_key"]);
                }
            }
            tracing::debug!("Effective config:\n{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        }

        LoadedConfig { config, models }
    }
}

fn redact(value: &mut serde_json::Value, path: &[&str]) {
    match path {
        [] => {}
        [key] => {
            if let Some(v) = value.get_mut(*key) && !v.is_null() {
                *v = serde_json::Value::String("[redacted]".into());
            }
        }
        [key, rest @ ..] => {
            if let Some(child) = value.get_mut(*key) {
                redact(child, rest);
            }
        }
    }
}

pub fn expand_env_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut var_name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var_name.push(c);
            }
            if let Ok(val) = std::env::var(&var_name) {
                result.push_str(&val);
            }
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars() {
        unsafe { std::env::set_var("TEST_KEY_123", "my-secret") };
        let result = expand_env_vars("key=${TEST_KEY_123}");
        assert_eq!(result, "key=my-secret");
        unsafe { std::env::remove_var("TEST_KEY_123") };
    }

    #[test]
    fn test_expand_env_vars_missing() {
        let result = expand_env_vars("key=${NONEXISTENT_VAR_XYZ}");
        assert_eq!(result, "key=");
    }

    #[test]
    fn defaults_are_sensible() {
        let config = Config::default();
        assert_eq!(config.server.port, 3001);
        assert_eq!(config.auth.encryption_secret, "dev-secret-change-in-production");
        assert_eq!(config.database.path, "data/db");
        assert_eq!(config.storage.workspaces_path, "data/workspaces");
        assert_eq!(config.scheduler.space_compaction_secs, 3600);
        assert!(!config.sso.enabled);
        assert!(config.sso.signups_match_email);
        assert!(config.browser.is_none());
        assert!(config.server.cors_origins.is_none());
        assert!(config.server.base_url.is_none());
        assert_eq!(config.server.max_body_size_bytes, 104_857_600);
        assert!(config.search.provider.is_none());
        assert!(config.search.searxng_base_url.is_none());
        assert_eq!(config.inference.max_tool_turns, 200);
        assert_eq!(config.inference.default_max_tokens, 8192);
        assert_eq!(config.inference.compaction_trigger_pct, 80);
        assert_eq!(config.inference.history_truncation_pct, 90);
    }

    #[test]
    fn env_var_overrides_multi_word_field() {
        // The key remapping (replace first _ with __) means FRONA_BROWSER_WS_URL
        // becomes browser__ws_url, which separator("__") resolves to browser.ws_url.
        unsafe { std::env::set_var("FRONA_BROWSER_WS_URL", "ws://custom:9999") };
        let loaded = Config::load();
        assert_eq!(loaded.config.browser.as_ref().unwrap().ws_url, "ws://custom:9999");
        unsafe { std::env::remove_var("FRONA_BROWSER_WS_URL") };
    }

    #[test]
    fn env_var_overrides_server_port() {
        unsafe { std::env::set_var("FRONA_SERVER_PORT", "9999") };
        let loaded = Config::load();
        assert_eq!(loaded.config.server.port, 9999);
        unsafe { std::env::remove_var("FRONA_SERVER_PORT") };
    }

    #[test]
    fn env_var_overrides_database_path() {
        unsafe { std::env::set_var("FRONA_DATABASE_PATH", "/tmp/testdb") };
        let loaded = Config::load();
        assert_eq!(loaded.config.database.path, "/tmp/testdb");
        unsafe { std::env::remove_var("FRONA_DATABASE_PATH") };
    }

    #[test]
    fn env_var_overrides_sso_enabled() {
        unsafe { std::env::set_var("FRONA_SSO_ENABLED", "true") };
        let loaded = Config::load();
        assert!(loaded.config.sso.enabled);
        unsafe { std::env::remove_var("FRONA_SSO_ENABLED") };
    }

    #[test]
    fn browser_config_ws_url_for_profile() {
        let config = BrowserConfig { ws_url: "ws://localhost:3333".into(), ..Default::default() };
        let url = config.ws_url_for_profile("alice", "google");
        assert!(url.starts_with("ws://localhost:3333?--user-data-dir="));
        assert!(url.contains("alice"));
        assert!(url.contains("google"));
    }

    #[test]
    fn browser_config_http_base_url() {
        let config = BrowserConfig { ws_url: "ws://localhost:3333".into(), ..Default::default() };
        assert_eq!(config.http_base_url(), "http://localhost:3333");
    }

    #[test]
    fn browser_config_profile_path() {
        let config = BrowserConfig {
            profiles_path: "/data/profiles".into(),
            ..Default::default()
        };
        let path = config.profile_path("bob", "github");
        assert_eq!(path, PathBuf::from("/data/profiles/bob/github"));
    }
}
