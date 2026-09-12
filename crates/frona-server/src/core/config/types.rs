use std::collections::HashMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::deserialize_bool_from_anything;
use surrealdb::types::SurrealValue;

use crate::core::Handle;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ServerConfig {
    #[schemars(description = "Port the server listens on.")]
    pub port: u16,
    #[schemars(description = "Path to the static frontend build directory.")]
    pub static_dir: String,
    #[schemars(description = "Issuer URL for JWT tokens.")]
    pub issuer_url: String,
    #[schemars(description = "Maximum number of concurrent background tasks.")]
    pub max_concurrent_tasks: usize,
    #[schemars(description = "Comma-separated list of allowed CORS origins.")]
    pub cors_origins: Option<String>,
    #[schemars(description = "Public base URL for the server (used for callbacks, links).")]
    pub base_url: Option<String>,
    #[schemars(description = "Override URL for the backend API (if different from base_url).")]
    pub backend_url: Option<String>,
    #[schemars(description = "Override URL for the frontend (if different from base_url).")]
    pub frontend_url: Option<String>,
    #[schemars(
        description = "Externally-reachable URL of this server (e.g. ngrok tunnel, public domain). Used as the default callback target for inbound webhooks and external service callbacks when no per-feature override is set."
    )]
    pub external_url: Option<String>,
    #[schemars(description = "Maximum request body size in bytes.")]
    pub max_body_size_bytes: usize,
    #[schemars(
        description = "Graceful shutdown timeout in seconds. Server force-exits after this duration."
    )]
    pub shutdown_timeout_secs: u64,
    #[schemars(
        description = "Seconds to buffer SSE events after a client disconnects, allowing reconnects to receive missed events. 0 disables."
    )]
    pub sse_pending_events_secs: u64,
    #[schemars(
        description = "Server-default IANA timezone (e.g. \"America/Los_Angeles\"). Used when a user has no timezone set and no per-task override is provided. Leave empty to auto-detect from TZ env var, /etc/localtime, or fall back to UTC."
    )]
    pub timezone: String,
}

impl ServerConfig {
    pub fn public_base_url(&self) -> String {
        self.backend_url
            .as_deref()
            .or(self.base_url.as_deref())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string()
    }

    pub fn public_frontend_url(&self) -> String {
        self.frontend_url
            .as_deref()
            .or(self.base_url.as_deref())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string()
    }

    pub fn external_base_url(&self) -> Option<String> {
        self.external_url
            .as_deref()
            .or(self.backend_url.as_deref())
            .or(self.base_url.as_deref())
            .map(|s| s.trim_end_matches('/').to_string())
    }

    /// Always returns an openable URL (unlike `public_base_url()` which may be empty).
    pub fn external_or_local_base_url(&self) -> String {
        self.external_base_url()
            .unwrap_or_else(|| format!("http://localhost:{}", self.port))
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 3001,
            static_dir: "/app/static".into(),
            issuer_url: String::new(),
            max_concurrent_tasks: 10,
            cors_origins: None,
            base_url: None,
            backend_url: None,
            frontend_url: None,
            external_url: None,
            max_body_size_bytes: 104_857_600,
            shutdown_timeout_secs: 60,
            sse_pending_events_secs: 60,
            timezone: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
#[serde(default)]
pub struct SandboxLimits {
    #[schemars(
        description = "Per-principal CPU usage limit as percentage of total system CPU. Kill sandboxed process if exceeded."
    )]
    pub max_cpu_pct: f64,
    #[schemars(
        description = "Per-principal memory usage limit as percentage of total system memory. Kill sandboxed process if exceeded."
    )]
    pub max_memory_pct: f64,
    #[schemars(
        description = "Default timeout in seconds for sandboxed execution. 0 means no timeout."
    )]
    pub timeout_secs: u64,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_cpu_pct: 95.0,
            max_memory_pct: 80.0,
            timeout_secs: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct SandboxConfig {
    #[schemars(
        description = "Disable filesystem sandboxing for CLI tools. Enable only if your OS does not support Landlock."
    )]
    pub disabled: bool,
    #[serde(flatten)]
    pub default_limits: SandboxLimits,
    #[schemars(
        description = "Global CPU usage limit across all sandboxed processes as percentage of total system CPU."
    )]
    pub max_total_cpu_pct: f64,
    #[schemars(
        description = "Global memory usage limit across all sandboxed processes as percentage of total system memory."
    )]
    pub max_total_memory_pct: f64,
    #[schemars(
        description = "Grant all sandbox principals outbound network access by default. Override with forbid policies."
    )]
    pub default_network_access: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            default_limits: SandboxLimits::default(),
            max_total_cpu_pct: 98.0,
            max_total_memory_pct: 90.0,
            default_network_access: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AuthConfig {
    #[schemars(description = "Secret key for JWT signing. Change from default in production.")]
    pub encryption_secret: String,
    #[schemars(description = "Access token lifetime in seconds.")]
    pub access_token_expiry_secs: u64,
    #[schemars(description = "Refresh token lifetime in seconds.")]
    pub refresh_token_expiry_secs: u64,
    #[schemars(description = "Presigned URL expiry in seconds.")]
    pub presign_expiry_secs: u64,
    #[schemars(
        description = "Ephemeral principal token lifetime in seconds (stateless; injected into sandboxed processes)."
    )]
    pub ephemeral_token_expiry_secs: u64,
    #[schemars(
        description = "Allow anyone to sign up from the registration page. When off, only admins can add users."
    )]
    pub allow_registration: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            encryption_secret: "dev-secret-change-in-production".into(),
            access_token_expiry_secs: 900,
            refresh_token_expiry_secs: 604800,
            presign_expiry_secs: 86400,
            ephemeral_token_expiry_secs: 300,
            allow_registration: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct SsoConfig {
    #[schemars(description = "Enable SSO/OIDC authentication.")]
    pub enabled: bool,
    #[schemars(description = "OIDC authority/issuer URL (e.g. https://accounts.google.com).")]
    pub authority: Option<String>,
    #[schemars(description = "OIDC client ID.")]
    pub client_id: Option<String>,
    #[schemars(description = "OIDC client secret.")]
    pub client_secret: Option<String>,
    #[schemars(description = "OIDC scopes to request.")]
    pub scopes: String,
    #[schemars(description = "Allow verification of emails not matching known users.")]
    pub allow_unknown_email_verification: bool,
    #[schemars(description = "Client cache expiration in seconds.")]
    pub client_cache_expiration: u64,
    #[schemars(description = "Disable local (email/password) authentication when SSO is enabled.")]
    pub disable_local_auth: bool,
    #[schemars(description = "Match SSO signups to existing users by email.")]
    pub signups_match_email: bool,
}

impl Default for SsoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            authority: None,
            client_id: None,
            client_secret: None,
            scopes: "openid email".into(),
            allow_unknown_email_verification: true,
            client_cache_expiration: 0,
            disable_local_auth: false,
            signups_match_email: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct DatabaseConfig {
    #[schemars(description = "Path to the SurrealDB data directory.")]
    pub path: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: "data/db".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct BrowserConfig {
    #[schemars(description = "WebSocket URL for browserless (e.g. ws://browserless:3333).")]
    pub ws_url: String,
    #[schemars(description = "Authentication token for the browserless HTTP API.")]
    #[serde(default)]
    pub api_token: Option<String>,
    #[schemars(description = "Path to store browser profiles.")]
    pub profiles_path: String,
    #[schemars(description = "Browser connection timeout in milliseconds.")]
    pub connection_timeout_ms: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            ws_url: String::new(),
            api_token: None,
            profiles_path: "/profiles".into(),
            connection_timeout_ms: 30000,
        }
    }
}

impl BrowserConfig {
    pub fn http_base_url(&self) -> String {
        self.ws_url
            .replace("ws://", "http://")
            .replace("wss://", "https://")
    }

    /// Browserless v2 requires a `token` query param on management endpoints
    /// (`/sessions`, `/kill`) even when no TOKEN env var is configured server-side.
    /// Falls back to "frona" which satisfies the schema validation.
    pub fn api_token(&self) -> &str {
        self.api_token.as_deref().unwrap_or("frona")
    }

    pub fn debugger_url_for_credential(&self, credential_id: &str) -> String {
        format!("/api/browser/debugger/{credential_id}")
    }

    pub fn profile_path(&self, handle: &crate::core::Handle, provider: &str) -> PathBuf {
        PathBuf::from(&self.profiles_path)
            .join(handle.as_ref())
            .join(provider)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct SearchConfig {
    #[schemars(description = "Search provider (searxng, tavily, or brave).")]
    pub provider: Option<String>,
    #[schemars(description = "Base URL for SearXNG instance.")]
    pub searxng_base_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct StorageConfig {
    #[schemars(
        description = "Root data directory. Per-user state lives at `{data_dir}/users/{user_handle}/...`."
    )]
    pub data_dir: String,
    #[schemars(
        description = "Path to shared configuration resources (read-only, ships with the binary)."
    )]
    pub shared_config_dir: String,
    #[schemars(description = "Path for installed skills directory.")]
    pub skills_dir: String,
    #[schemars(description = "Path for system cache directory.")]
    pub cache_dir: String,
    #[schemars(
        description = "Path for your own ontologies. Loaded alongside the bundled \
        ones and trusted the same, so a file here can retype or untype pages. Need not \
        exist."
    )]
    pub ontology_dir: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: "data".into(),
            shared_config_dir: "resources".into(),
            skills_dir: "data/skills".into(),
            cache_dir: "data/system/cache".into(),
            ontology_dir: "data/ontology".into(),
        }
    }
}

impl StorageConfig {
    /// The two directories the ontology catalogue is assembled from.
    ///
    /// The bundled half is derived from `shared_config_dir` rather than configurable:
    /// it is image content, read-only, and replaced wholesale by an upgrade. In a
    /// container it is a layer, so nothing may be written there - it would vanish on
    /// restart. Anything fetched at runtime belongs in `ontology_dir`, which sits on
    /// the data volume and persists.
    ///
    /// They stay separate rather than merging into one because source attribution is
    /// assigned on first sight, and "gone because a newer image replaced it" has to
    /// remain distinguishable from "the user deleted it" - one is an upgrade, the other
    /// is intent.
    pub fn ontology_roots(&self) -> crate::memory::pkm::ontology::Roots {
        crate::memory::pkm::ontology::Roots {
            release: PathBuf::from(&self.shared_config_dir).join("ontology"),
            user: PathBuf::from(&self.ontology_dir),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct SchedulerConfig {
    #[schemars(description = "Scheduler poll interval in seconds.")]
    pub poll_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { poll_secs: 60 }
    }
}

#[derive(
    Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, surrealdb::types::SurrealValue,
)]
#[surreal(crate = "surrealdb::types")]
#[serde(default)]
pub struct RetryConfig {
    #[schemars(description = "Maximum number of retry attempts. 0 disables retry.")]
    pub max_retries: u32,
    #[schemars(description = "Initial backoff delay in milliseconds.")]
    pub initial_backoff_ms: u64,
    #[schemars(description = "Multiplier applied to backoff delay between retries.")]
    pub backoff_multiplier: f64,
    #[schemars(description = "Maximum backoff delay in milliseconds.")]
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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ShareConfig {
    #[schemars(description = "TTL in seconds for newly-issued Share rows (short links).")]
    pub ttl_secs: u64,
    #[schemars(description = "Interval in seconds between expired-share cleanup runs.")]
    pub cleanup_interval_secs: u64,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 30 * 24 * 60 * 60,
            cleanup_interval_secs: 6 * 60 * 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ChannelConfig {
    #[schemars(
        description = "Default retry policy for failed channel connections. Per-channel overrides take precedence."
    )]
    pub retry: RetryConfig,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            retry: RetryConfig {
                max_retries: u32::MAX,
                initial_backoff_ms: 1_000,
                backoff_multiplier: 2.0,
                max_backoff_ms: 60_000,
            },
        }
    }
}

#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
#[serde(default)]
pub struct CommonModelFields {
    #[schemars(description = "Model ID (without provider prefix).")]
    #[parameter(skip)]
    pub model: String,
    #[serde(default)]
    #[schemars(description = "Fallback models tried in order if the primary fails.")]
    #[parameter(skip)]
    pub fallbacks: Vec<ModelGroupConfig>,
    #[serde(default)]
    #[schemars(description = "Maximum tokens to generate per response.")]
    #[parameter(
        bedrock = "inferenceConfig.maxTokens",
        open_ai_chat = "max_completion_tokens",
        responses = "max_output_tokens",
        gemini = "generationConfig.maxOutputTokens",
        ollama = "options.num_predict"
    )]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    #[schemars(description = "Sampling temperature (0.0-2.0).")]
    #[parameter(
        bedrock = "inferenceConfig.temperature",
        gemini = "generationConfig.temperature",
        ollama = "options.temperature"
    )]
    pub temperature: Option<f64>,
    #[serde(default)]
    #[schemars(description = "Context window size override.")]
    #[parameter(skip)]
    pub context_window: Option<usize>,
    #[serde(default)]
    #[schemars(description = "Retry configuration for this model group.")]
    #[parameter(skip)]
    pub retry: RetryConfig,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    #[schemars(description = "Additional native request parameters for the selected protocol.")]
    #[parameter(skip)]
    pub extra_params: serde_json::Map<String, serde_json::Value>,
}

#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
#[serde(default, deny_unknown_fields)]
pub struct AnthropicThinking {
    #[serde(rename = "type")]
    #[schemars(description = "'enabled' or 'disabled'.")]
    pub thinking_type: String,
    #[serde(default)]
    #[schemars(description = "Token budget for thinking (required when type is 'enabled').")]
    pub budget_tokens: Option<u64>,
}

#[serde_with::skip_serializing_none]
#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
pub struct OpenAICompatParams {
    pub top_p: Option<f64>,
    #[parameter(responses = false)]
    pub min_p: Option<f64>,
    #[parameter(responses = false)]
    pub frequency_penalty: Option<f64>,
    #[parameter(responses = false)]
    pub presence_penalty: Option<f64>,
    #[parameter(responses = false)]
    pub seed: Option<i64>,
    #[parameter(responses = "max_output_tokens")]
    pub max_completion_tokens: Option<u64>,
    #[schemars(description = "Reasoning effort level (e.g. 'low', 'medium', 'high').")]
    #[parameter(responses = "reasoning.effort")]
    pub reasoning_effort: Option<String>,
    #[parameter(responses = false)]
    pub logprobs: Option<bool>,
    pub top_logprobs: Option<u64>,
    #[parameter(responses = false)]
    pub stop: Option<Vec<String>>,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata)]
#[serde(deny_unknown_fields)]
pub struct GeminiThinkingConfig {
    #[parameter(path = "thinkingBudget")]
    pub thinking_budget: u64,
    #[parameter(path = "includeThoughts")]
    pub include_thoughts: Option<bool>,
}

#[serde_with::skip_serializing_none]
#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
pub struct AnthropicParams {
    #[parameter(nested)]
    pub thinking: Option<AnthropicThinking>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop_sequences: Option<Vec<String>>,
}

#[serde_with::skip_serializing_none]
#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
#[parameter(prefix = "options")]
pub struct OllamaParams {
    #[parameter(root)]
    pub think: Option<bool>,
    pub num_ctx: Option<u64>,
    pub num_predict: Option<u64>,
    pub num_batch: Option<u64>,
    pub num_keep: Option<i64>,
    pub num_thread: Option<u64>,
    pub num_gpu: Option<u64>,
    pub top_k: Option<u64>,
    pub top_p: Option<f64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub repeat_last_n: Option<i64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub mirostat: Option<u64>,
    pub mirostat_eta: Option<f64>,
    pub mirostat_tau: Option<f64>,
    pub tfs_z: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Option<Vec<String>>,
    pub use_mmap: Option<bool>,
    pub use_mlock: Option<bool>,
}

#[serde_with::skip_serializing_none]
#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
#[parameter(prefix = "generationConfig")]
pub struct GeminiParams {
    #[parameter(path = "thinkingConfig", nested)]
    pub thinking_config: Option<GeminiThinkingConfig>,
    #[parameter(path = "topP")]
    pub top_p: Option<f64>,
    #[parameter(path = "topK")]
    pub top_k: Option<u64>,
    #[parameter(path = "stopSequences")]
    pub stop_sequences: Option<Vec<String>>,
    #[parameter(path = "candidateCount")]
    pub candidate_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApi {
    #[default]
    ChatCompletions,
    Responses,
}

/// Stable request protocol names persisted in YAML and returned by authoring APIs.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash)]
pub enum ApiSurface {
    #[serde(
        rename = "completions",
        alias = "chat_completions",
        alias = "openai-chat-completions"
    )]
    Completions,
    #[serde(rename = "responses", alias = "openai-responses")]
    Responses,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "google-generate-content")]
    GoogleGenerateContent,
    #[serde(rename = "amazon-bedrock-converse")]
    AmazonBedrockConverse,
    #[serde(rename = "cohere-chat")]
    CohereChat,
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "huggingface")]
    HuggingFace,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterId {
    Openai,
    Anthropic,
    Gemini,
    Bedrock,
    Cohere,
    Ollama,
    Huggingface,
}

impl AdapterId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::Bedrock => "bedrock",
            Self::Cohere => "cohere",
            Self::Ollama => "ollama",
            Self::Huggingface => "huggingface",
        }
    }
}

impl From<OpenAiApi> for ApiSurface {
    fn from(value: OpenAiApi) -> Self {
        match value {
            OpenAiApi::ChatCompletions => Self::Completions,
            OpenAiApi::Responses => Self::Responses,
        }
    }
}

impl TryFrom<ApiSurface> for OpenAiApi {
    type Error = &'static str;

    fn try_from(value: ApiSurface) -> Result<Self, Self::Error> {
        match value {
            ApiSurface::Completions => Ok(Self::ChatCompletions),
            ApiSurface::Responses => Ok(Self::Responses),
            _ => Err("protocol is not an OpenAI request surface"),
        }
    }
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "provider")]
pub enum ProviderModel {
    #[serde(rename = "bedrock")]
    Bedrock {
        #[serde(flatten)]
        params: BedrockParams,
    },
    #[serde(rename = "anthropic")]
    Anthropic {
        #[serde(flatten)]
        params: AnthropicParams,
    },
    #[serde(rename = "ollama")]
    Ollama {
        #[serde(flatten)]
        params: OllamaParams,
    },
    #[serde(rename = "openai")]
    OpenAI {
        api: Option<OpenAiApi>,
        #[serde(flatten)]
        params: OpenAICompatParams,
    },
    #[serde(rename = "groq")]
    Groq {
        #[serde(flatten)]
        params: OpenAICompatParams,
    },
    #[serde(rename = "openrouter")]
    OpenRouter {
        #[serde(flatten)]
        params: OpenAICompatParams,
    },
    #[serde(rename = "deepseek")]
    DeepSeek {
        #[serde(flatten)]
        params: OpenAICompatParams,
    },
    #[serde(rename = "xai")]
    XAI {
        #[serde(flatten)]
        params: OpenAICompatParams,
    },
    #[serde(rename = "together")]
    Together {
        #[serde(flatten)]
        params: OpenAICompatParams,
    },
    #[serde(rename = "hyperbolic")]
    Hyperbolic {
        #[serde(flatten)]
        params: OpenAICompatParams,
    },
    #[serde(rename = "gemini")]
    Gemini {
        #[serde(flatten)]
        params: GeminiParams,
    },
    #[serde(rename = "generic")]
    #[default]
    Generic,
    #[serde(skip)]
    #[schemars(skip)]
    Custom { name: String },
}

impl From<&str> for ProviderModel {
    fn from(name: &str) -> Self {
        Self::from_name(name)
    }
}

impl From<String> for ProviderModel {
    fn from(name: String) -> Self {
        Self::from_name(&name)
    }
}

impl ProviderModel {
    pub fn from_name(name: &str) -> Self {
        match name {
            "bedrock" => Self::Bedrock {
                params: Default::default(),
            },
            "anthropic" => Self::Anthropic {
                params: Default::default(),
            },
            "ollama" => Self::Ollama {
                params: Default::default(),
            },
            "openai" => Self::OpenAI {
                api: None,
                params: Default::default(),
            },
            "groq" => Self::Groq {
                params: Default::default(),
            },
            "openrouter" => Self::OpenRouter {
                params: Default::default(),
            },
            "deepseek" => Self::DeepSeek {
                params: Default::default(),
            },
            "xai" => Self::XAI {
                params: Default::default(),
            },
            "together" => Self::Together {
                params: Default::default(),
            },
            "hyperbolic" => Self::Hyperbolic {
                params: Default::default(),
            },
            "gemini" => Self::Gemini {
                params: Default::default(),
            },
            "generic" => Self::Generic,
            name => Self::Custom {
                name: name.to_string(),
            },
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Bedrock { .. } => "bedrock",
            Self::Anthropic { .. } => "anthropic",
            Self::Ollama { .. } => "ollama",
            Self::OpenAI { .. } => "openai",
            Self::Groq { .. } => "groq",
            Self::OpenRouter { .. } => "openrouter",
            Self::DeepSeek { .. } => "deepseek",
            Self::XAI { .. } => "xai",
            Self::Together { .. } => "together",
            Self::Hyperbolic { .. } => "hyperbolic",
            Self::Gemini { .. } => "gemini",
            Self::Generic => "generic",
            Self::Custom { name } => name,
        }
    }
}

#[serde_with::skip_serializing_none]
#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
#[serde(default, deny_unknown_fields)]
#[parameter(prefix = "inferenceConfig")]
pub struct BedrockParams {
    #[parameter(path = "topP")]
    pub top_p: Option<f64>,
    #[parameter(path = "stopSequences")]
    pub stop_sequences: Option<Vec<String>>,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ModelSettings {
    pub thinking: Option<AnthropicThinking>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop_sequences: Option<Vec<String>>,
    pub think: Option<bool>,
    pub num_ctx: Option<u64>,
    pub num_predict: Option<u64>,
    pub num_batch: Option<u64>,
    pub num_keep: Option<i64>,
    pub num_thread: Option<u64>,
    pub num_gpu: Option<u64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub repeat_last_n: Option<i64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub mirostat: Option<u64>,
    pub mirostat_eta: Option<f64>,
    pub mirostat_tau: Option<f64>,
    pub tfs_z: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Option<Vec<String>>,
    pub use_mmap: Option<bool>,
    pub use_mlock: Option<bool>,
    pub max_completion_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub logprobs: Option<bool>,
    pub top_logprobs: Option<u64>,
    pub thinking_config: Option<GeminiThinkingConfig>,
    pub candidate_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelGroupConfig {
    pub provider: Handle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiSurface>,
    #[serde(flatten)]
    pub common: CommonModelFields,
    #[serde(flatten)]
    pub settings: ModelSettings,
}

impl Default for ModelGroupConfig {
    fn default() -> Self {
        Self {
            provider: Handle::const_validated("generic"),
            api: None,
            common: CommonModelFields::default(),
            settings: ModelSettings::default(),
        }
    }
}

impl ModelGroupConfig {
    pub fn common(&self) -> &CommonModelFields {
        &self.common
    }

    pub fn provider_name(&self) -> &str {
        self.provider.as_str()
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ModelProviderConfig {
    #[schemars(description = "Stable ID of an authorized managed credential.")]
    #[schemars(with = "Option<String>")]
    pub credential_id: Option<uuid::Uuid>,
    #[schemars(description = "API key for this provider. Supports ${ENV_VAR} references.")]
    pub api_key: Option<String>,
    #[schemars(description = "Custom base URL for this provider's API.")]
    pub base_url: Option<String>,
    #[serde(
        default = "serde_aux::prelude::bool_true",
        deserialize_with = "deserialize_bool_from_anything"
    )]
    #[schemars(description = "Whether this provider is enabled.")]
    pub enabled: bool,
    #[schemars(description = "Provider brand. Legacy entries infer it from the map key.")]
    pub provider: Option<String>,
    #[schemars(description = "Compiled logical adapter used for dynamic or direct-YAML brands.")]
    pub adapter: Option<AdapterId>,
    pub aws_profile: Option<String>,
    pub aws_region: Option<String>,
    pub azure_credential: Option<String>,
    #[serde(flatten)]
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

impl std::fmt::Debug for ModelProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelProviderConfig")
            .field("provider", &self.provider)
            .field("adapter", &self.adapter)
            .field("enabled", &self.enabled)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl Default for ModelProviderConfig {
    fn default() -> Self {
        Self {
            credential_id: None,
            api_key: None,
            base_url: None,
            enabled: true,
            provider: None,
            adapter: None,
            aws_profile: None,
            aws_region: None,
            azure_credential: None,
            attributes: serde_json::Map::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct InferenceConfig {
    #[schemars(description = "Maximum number of tool-use turns per inference loop.")]
    pub max_tool_turns: usize,
    #[schemars(description = "Default max tokens when not specified by model group.")]
    pub default_max_tokens: u64,
    #[schemars(description = "Percentage of context window usage that triggers compaction.")]
    pub compaction_trigger_pct: usize,
    #[schemars(description = "Percentage of history to keep after truncation.")]
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

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct VoiceConfig {
    #[schemars(description = "Voice provider (twilio or none).")]
    pub provider: Option<String>,
    #[schemars(description = "Twilio account SID.")]
    pub twilio_account_sid: Option<String>,
    #[schemars(description = "Twilio auth token.")]
    pub twilio_auth_token: Option<String>,
    #[schemars(description = "Twilio phone number to call from.")]
    pub twilio_from_number: Option<String>,
    #[schemars(description = "Twilio voice ID for text-to-speech.")]
    pub twilio_voice_id: Option<String>,
    #[schemars(description = "Twilio speech recognition model.")]
    pub twilio_speech_model: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct VaultConfig {
    #[schemars(description = "1Password service account token (for the `op` CLI).")]
    pub onepassword_service_account_token: Option<String>,
    #[schemars(description = "1Password vault ID.")]
    pub onepassword_vault_id: Option<String>,
    #[schemars(description = "Bitwarden CLI client ID (personal API key).")]
    pub bitwarden_client_id: Option<String>,
    #[schemars(description = "Bitwarden CLI client secret (personal API key).")]
    pub bitwarden_client_secret: Option<String>,
    #[schemars(description = "Bitwarden master password (for vault unlock).")]
    pub bitwarden_master_password: Option<String>,
    #[schemars(
        description = "Bitwarden server URL (for self-hosted instances, leave empty for cloud)."
    )]
    pub bitwarden_server_url: Option<String>,
    #[schemars(description = "HashiCorp Vault server address.")]
    pub hashicorp_address: Option<String>,
    #[schemars(description = "HashiCorp Vault access token.")]
    pub hashicorp_token: Option<String>,
    #[schemars(description = "HashiCorp Vault secrets mount path.")]
    pub hashicorp_mount: Option<String>,
    #[schemars(description = "Path to KeePass database file.")]
    pub keepass_path: Option<String>,
    #[schemars(description = "KeePass database password.")]
    pub keepass_password: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AppConfig {
    #[schemars(description = "Start of port range for managed apps.")]
    pub port_range_start: u16,
    #[schemars(description = "End of port range for managed apps.")]
    pub port_range_end: u16,
    #[schemars(description = "Health check timeout in seconds.")]
    pub health_check_timeout_secs: u64,
    #[schemars(description = "Maximum process restart attempts before marking as failed.")]
    pub max_restart_attempts: u32,
    #[schemars(description = "Seconds of inactivity before an app is auto-hibernated.")]
    pub hibernate_after_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port_range_start: 4000,
            port_range_end: 4100,
            health_check_timeout_secs: 30,
            max_restart_attempts: 2,
            hibernate_after_secs: 259200,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct CacheConfig {
    #[schemars(description = "TTL in seconds for cached entities (agents, users).")]
    pub entity_ttl_secs: u64,
    #[schemars(description = "Maximum number of cached entities.")]
    pub entity_max_capacity: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            entity_ttl_secs: 300,
            entity_max_capacity: 1000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct McpConfig {
    #[schemars(description = "Whether MCP server support is enabled.")]
    pub enabled: bool,
    #[schemars(
        description = "Path for shared package caches (npm, uv). Defaults to `{data_dir}/system/mcp-cache`."
    )]
    #[serde(default)]
    pub cache_path: Option<String>,
    #[schemars(description = "Maximum number of MCP servers a user may have installed.")]
    pub max_servers_per_user: u32,
    #[schemars(
        description = "Seconds to wait for an MCP server's initialize handshake before failing."
    )]
    pub startup_timeout_secs: u64,
    #[schemars(description = "Interval in seconds between MCP server liveness checks.")]
    pub health_check_interval_secs: u64,
    #[schemars(
        description = "Maximum process restart attempts before marking an MCP server as failed."
    )]
    pub max_restart_attempts: u32,
    #[schemars(description = "Default transport for new MCP servers: 'stdio' or 'http'.")]
    pub default_transport: String,
    #[schemars(description = "Start of the port range for local HTTP MCP servers.")]
    pub port_range_start: u16,
    #[schemars(description = "End of the port range for local HTTP MCP servers (exclusive).")]
    pub port_range_end: u16,
    #[schemars(
        description = "When true, expose MCP tools via the mcpctl CLI bridge instead of individual tool definitions. Reduces LLM context token usage."
    )]
    pub bridge_mode: bool,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_path: None,
            max_servers_per_user: 32,
            startup_timeout_secs: 30,
            health_check_interval_secs: 10,
            max_restart_attempts: 3,
            default_transport: "stdio".into(),
            port_range_start: 4100,
            port_range_end: 4200,
            bridge_mode: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default, JsonSchema)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub sandbox: SandboxConfig,
    pub auth: AuthConfig,
    pub sso: SsoConfig,
    pub database: DatabaseConfig,
    pub browser: Option<BrowserConfig>,
    pub search: SearchConfig,
    pub vault: VaultConfig,
    pub storage: StorageConfig,
    pub scheduler: SchedulerConfig,
    pub inference: InferenceConfig,
    pub memory: MemoryConfig,
    pub voice: VoiceConfig,
    pub app: AppConfig,
    pub cache: CacheConfig,
    pub mcp: McpConfig,
    #[serde(default)]
    pub channel: ChannelConfig,
    #[serde(default)]
    pub share: ShareConfig,
    #[serde(default)]
    pub signal: SignalConfig,
    #[serde(default)]
    pub models: HashMap<String, ModelGroupConfig>,
    #[serde(default, deserialize_with = "deserialize_provider_configs")]
    pub providers: HashMap<Handle, ModelProviderConfig>,
}

fn deserialize_provider_configs<'de, D>(
    deserializer: D,
) -> Result<HashMap<Handle, ModelProviderConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ProviderMapVisitor;

    impl<'de> serde::de::Visitor<'de> for ProviderMapVisitor {
        type Value = HashMap<Handle, ModelProviderConfig>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of provider handles to provider configurations")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut providers = HashMap::with_capacity(map.size_hint().unwrap_or(0));
            while let Some((handle, provider)) = map.next_entry::<Handle, ModelProviderConfig>()? {
                if providers.insert(handle.clone(), provider).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "provider handle '{}' collides after trimming and lowercasing",
                        handle
                    )));
                }
            }
            Ok(providers)
        }
    }

    deserializer.deserialize_map(ProviderMapVisitor)
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct SignalConfig {
    #[schemars(description = "Maximum number of pending signal watches per user.")]
    pub max_pending_per_user: usize,
    #[schemars(
        description = "Default safety cap on the number of candidates a one-shot watch can be evaluated against before auto-failing. Sweep cadence is driven by scheduler.poll_secs."
    )]
    pub default_max_evaluations: u32,
    #[schemars(
        description = "Default safety cap on the number of fires a continuous-mode watch can absorb before auto-completing. Higher than the one-shot default because continuous watches stream over time."
    )]
    pub default_max_continuous_evaluations: u32,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            max_pending_per_user: 50,
            default_max_evaluations: 50,
            default_max_continuous_evaluations: 1_000,
        }
    }
}

/// Which memory backend runs. `basic` rolls loose
/// `memory_entry` rows into compacted summaries; `pkm` builds a knowledge base
/// of pages from background consolidation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBackend {
    #[default]
    Basic,
    Pkm,
}

/// Memory subsystem configuration. Flat (single level under `memory`) so every
/// knob is reachable via `FRONA_MEMORY_*` env overrides, not just YAML.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct MemoryConfig {
    #[schemars(
        description = "Which memory backend to run: `basic` or `pkm`. Unset (null) \
        resolves to `basic` at boot; the setup wizard writes `pkm` for fresh installs and \
        existing installs opt in explicitly, so upgrades stay on `basic` unless changed."
    )]
    pub backend: Option<MemoryBackend>,
    #[schemars(
        description = "Model group for memory background work. Basic compaction falls back to the chat agent model; pkm consolidation falls back to `primary` when the group is undefined."
    )]
    pub model_group: String,
    #[schemars(description = "basic: skip user/agent memory compaction below this many tokens.")]
    pub basic_compaction_token_threshold: usize,
    #[schemars(
        description = "basic: interval in seconds between user/agent memory compaction runs."
    )]
    pub basic_compaction_secs: u64,
    #[schemars(description = "basic: interval in seconds between space memory compaction runs.")]
    pub basic_space_compaction_secs: u64,
    #[schemars(description = "pkm: max hits returned by `memory_search`.")]
    pub pkm_search_top_k: i64,
    #[schemars(description = "pkm: recency-decay half-life (seconds) for short memory.")]
    pub pkm_short_memory_half_life_secs: u64,
    #[schemars(description = "pkm: drop short memory once its decay score falls below this.")]
    pub pkm_short_memory_demote_threshold: f32,
    #[schemars(description = "pkm: max short-memory lines injected into `<short_memory>`.")]
    pub pkm_short_memory_top_n: usize,
    #[schemars(description = "pkm: token budget for the `<short_memory>` block.")]
    pub pkm_short_memory_token_cap: usize,
    #[schemars(
        description = "pkm: token budget for the `<available_playbooks>` index; playbooks past the cap (lowest use_count first) are dropped."
    )]
    pub pkm_playbook_index_token_cap: usize,
    #[schemars(
        description = "pkm: how often (seconds) the consolidation sweep scans for idle chats."
    )]
    pub pkm_consolidate_secs: u64,
    #[schemars(
        description = "pkm: how long (seconds) a chat must be quiet before it's consolidated."
    )]
    pub pkm_consolidate_idle_secs: u64,
    #[schemars(
        description = "pkm: how many consolidation model calls run at once - chats being \
        mined by the sweep, and pages being authored. Bounded so a first run over a long \
        history does not open one request per chat/page simultaneously."
    )]
    pub pkm_consolidation_concurrency: usize,
    #[schemars(
        description = "pkm classify and resolve: maximum exploration-tool turns per structured conversation."
    )]
    pub pkm_consolidation_max_tool_turns: usize,
    #[schemars(
        description = "pkm classify, resolve, and reconcile: maximum structured submission attempts per conversation."
    )]
    pub pkm_consolidation_max_submissions: usize,
    #[schemars(
        description = "pkm playbook resolve and author: maximum exploration-tool turns per structured conversation."
    )]
    pub pkm_playbook_max_tool_turns: usize,
    #[schemars(
        description = "pkm playbook resolve and author: maximum structured submission attempts per conversation."
    )]
    pub pkm_playbook_max_submissions: usize,
    #[schemars(
        description = "pkm: maximum estimated transcript tokens sent to extract in one request."
    )]
    pub pkm_extract_max_tokens: usize,
    #[schemars(description = "pkm: maximum messages consumed by one extract request.")]
    pub pkm_extract_max_messages: usize,
    #[schemars(
        description = "pkm extract: number of same-chat Agent messages searched backward for successful tool evidence supporting an Agent-sourced memory."
    )]
    pub pkm_extract_agent_evidence_lookback_messages: usize,
    #[schemars(
        description = "pkm extract: token cap returned by each scoped tool-evidence search or read."
    )]
    pub pkm_extract_agent_evidence_result_token_cap: usize,
    #[schemars(
        description = "pkm: how many times a consolidation stage may fail before its pass \
        is abandoned. Attempts count the CURRENT stage and reset when the pass advances, \
        so a long pass that hiccups at several stages is not dropped for making progress."
    )]
    pub pkm_consolidation_max_attempts: u32,
    #[schemars(
        description = "pkm adjudication: maximum model submission attempts for each \
        adjudication batch, including the initial submission and guardrail revisions."
    )]
    pub pkm_adjudication_max_attempts_per_batch: usize,
    #[schemars(
        description = "pkm: fatal post-extraction checkpoint resets allowed before the pass is marked Failed. With 2, the first fatal failure restarts at Classify and the second fails terminally."
    )]
    pub pkm_consolidation_checkpoint_failure_cap: u32,
    #[schemars(
        description = "pkm: base backoff (seconds) between retries of a failed \
        consolidation pass; doubles per attempt. Retries are quantised by the sweep tick, \
        so a value below `pkm_consolidate_secs` buys nothing."
    )]
    pub pkm_consolidation_retry_base_secs: u64,
    #[schemars(
        description = "pkm: how many finished consolidation passes to keep per user as a \
        log. Older ones are dropped by the cleanup stage."
    )]
    pub pkm_consolidation_keep_records: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: None,
            model_group: crate::inference::ModelRef::MEMORY.as_str().into(),
            basic_compaction_token_threshold: 3_000,
            basic_compaction_secs: 7200,
            basic_space_compaction_secs: 3600,
            pkm_search_top_k: 8,
            pkm_short_memory_half_life_secs: 14 * 24 * 3600,
            pkm_short_memory_demote_threshold: 0.1,
            pkm_short_memory_top_n: 16,
            pkm_short_memory_token_cap: 3_000,
            pkm_playbook_index_token_cap: 1_500,
            pkm_consolidate_secs: 60,
            pkm_consolidate_idle_secs: 300,
            pkm_consolidation_concurrency: 4,
            pkm_consolidation_max_tool_turns: 8,
            pkm_consolidation_max_submissions: 8,
            pkm_playbook_max_tool_turns: 20,
            pkm_playbook_max_submissions: 20,
            pkm_extract_max_tokens: 10_000,
            pkm_extract_max_messages: 300,
            pkm_extract_agent_evidence_lookback_messages: 10,
            pkm_extract_agent_evidence_result_token_cap: 4_000,
            pkm_consolidation_max_attempts: 3,
            pkm_adjudication_max_attempts_per_batch: 40,
            pkm_consolidation_checkpoint_failure_cap: 2,
            pkm_consolidation_retry_base_secs: 120,
            pkm_consolidation_keep_records: 20,
        }
    }
}
