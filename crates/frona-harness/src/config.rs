use crate::Handle;
pub use crate::retry_config::RetryConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::deserialize_bool_from_anything;

#[derive(
    Debug, Clone, Default, Deserialize, Serialize, JsonSchema, frona_derive::ParameterMetadata,
)]
#[parameter(crate = "crate")]
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
#[parameter(crate = "crate")]
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
#[parameter(crate = "crate")]
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
#[parameter(crate = "crate")]
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
#[parameter(crate = "crate")]
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
#[parameter(crate = "crate")]
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
#[parameter(crate = "crate")]
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
#[parameter(crate = "crate")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMethod {
    ApiKey,
    Oauth,
    Aws,
    AzureEntra,
    Anonymous,
}

impl CredentialMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Oauth => "oauth",
            Self::Aws => "aws",
            Self::AzureEntra => "azure_entra",
            Self::Anonymous => "anonymous",
        }
    }
}
