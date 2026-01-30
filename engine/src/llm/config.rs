use std::collections::HashMap;

use serde::Deserialize;
use serde_aux::field_attributes::deserialize_bool_from_anything;

use super::error::LlmError;
use super::provider::ModelRef;

#[derive(Debug, Clone, Deserialize)]
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
            max_retries: 3,
            initial_backoff_ms: 500,
            backoff_multiplier: 2.0,
            max_backoff_ms: 30_000,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ModelRegistryConfig {
    #[serde(default)]
    pub providers: HashMap<String, ModelProviderConfig>,
    pub models: HashMap<String, ModelGroupConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ModelProviderConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    #[serde(
        default = "serde_aux::prelude::bool_true",
        deserialize_with = "deserialize_bool_from_anything"
    )]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Clone)]
pub struct ModelGroup {
    pub main: ModelRef,
    pub fallbacks: Vec<ModelRef>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub context_window: Option<usize>,
    pub retry: RetryConfig,
}

impl ModelRegistryConfig {
    pub fn load(path: &str) -> Result<Self, LlmError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| LlmError::ConfigError(format!("Failed to read {path}: {e}")))?;

        let expanded = expand_env_vars(&content);

        let config: ModelRegistryConfig = serde_json::from_str(&expanded)
            .map_err(|e| LlmError::ConfigError(format!("Failed to parse {path}: {e}")))?;

        Ok(config)
    }

    pub fn auto_discover() -> Self {
        let mut providers = HashMap::new();

        let known = [
            ("openai", "OPENAI_API_KEY"),
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("groq", "GROQ_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("gemini", "GEMINI_API_KEY"),
            ("cohere", "COHERE_API_KEY"),
            ("mistral", "MISTRAL_API_KEY"),
            ("perplexity", "PERPLEXITY_API_KEY"),
            ("together", "TOGETHER_API_KEY"),
            ("xai", "XAI_API_KEY"),
            ("hyperbolic", "HYPERBOLIC_API_KEY"),
            ("moonshot", "MOONSHOT_API_KEY"),
            ("mira", "MIRA_API_KEY"),
            ("galadriel", "GALADRIEL_API_KEY"),
            ("huggingface", "HUGGINGFACE_API_KEY"),
        ];

        for (name, env_var) in known {
            if let Ok(key) = std::env::var(env_var) {
                providers.insert(
                    name.to_string(),
                    ModelProviderConfig {
                        api_key: Some(key),
                        base_url: None,
                        enabled: true,
                    },
                );
            }
        }

        if std::env::var("OLLAMA_API_BASE_URL").is_ok() {
            providers.insert(
                "ollama".to_string(),
                ModelProviderConfig {
                    api_key: None,
                    base_url: std::env::var("OLLAMA_API_BASE_URL").ok(),
                    enabled: true,
                },
            );
        }

        let models = build_default_model_groups(&providers);

        Self { providers, models }
    }

    pub fn merge_with_auto_discovered(&mut self) {
        let discovered = Self::auto_discover();
        for (name, provider) in discovered.providers {
            self.providers.entry(name).or_insert(provider);
        }
    }

    pub fn parse_model_groups(&self) -> Result<HashMap<String, ModelGroup>, LlmError> {
        let mut groups = HashMap::new();

        for (name, config) in &self.models {
            let main = ModelRef::parse(&config.main)?;
            let fallbacks = config
                .fallbacks
                .iter()
                .map(|s| ModelRef::parse(s))
                .collect::<Result<Vec<_>, _>>()?;

            groups.insert(
                name.clone(),
                ModelGroup {
                    main,
                    fallbacks,
                    max_tokens: config.max_tokens,
                    temperature: config.temperature,
                    context_window: config.context_window,
                    retry: config.retry.clone(),
                },
            );
        }

        Ok(groups)
    }
}

fn expand_env_vars(input: &str) -> String {
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
                result.push_str(&val)
            }
        } else {
            result.push(c);
        }
    }

    result
}

fn default_model_for_provider(provider: &str) -> &str {
    match provider {
        "anthropic" => "claude-haiku-4-5",
        "openai" => "gpt-4o",
        "groq" => "llama-3.3-70b-versatile",
        "deepseek" => "deepseek-chat",
        "gemini" => "gemini-2.0-flash",
        "mistral" => "mistral-large-latest",
        "cohere" => "command-r-plus",
        "xai" => "grok-2-latest",
        "ollama" => "qwen3-vl:32b",
        _ => "default",
    }
}

fn build_default_model_groups(
    providers: &HashMap<String, ModelProviderConfig>,
) -> HashMap<String, ModelGroupConfig> {
    let mut models = HashMap::new();

    if let Some((provider, _)) = providers.iter().next() {
        let model = default_model_for_provider(provider);
        let main = format!("{provider}/{model}");
        models.insert(
            "primary".to_string(),
            ModelGroupConfig {
                main,
                fallbacks: vec![],
                max_tokens: Some(8192),
                temperature: None,
                context_window: None,
                retry: RetryConfig::default(),
            },
        );
    }

    models
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
    fn test_model_ref_parse() {
        let r = ModelRef::parse("anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(r.provider, "anthropic");
        assert_eq!(r.model_id, "claude-sonnet-4-5");
    }

    #[test]
    fn test_model_ref_parse_invalid() {
        assert!(ModelRef::parse("no-slash").is_err());
        assert!(ModelRef::parse("/missing-provider").is_err());
        assert!(ModelRef::parse("missing-model/").is_err());
    }
}
