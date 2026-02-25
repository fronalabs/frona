use std::collections::HashMap;

pub use crate::core::config::{InferenceConfig, ModelGroupConfig, ModelProviderConfig, RetryConfig};

use super::error::InferenceError;
use super::provider::ModelRef;

#[derive(Debug, Clone)]
pub struct ModelGroup {
    pub name: String,
    pub main: ModelRef,
    pub fallbacks: Vec<ModelRef>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub context_window: Option<usize>,
    pub retry: RetryConfig,
    pub inference: InferenceConfig,
}

#[derive(Debug)]
pub struct ModelRegistryConfig {
    pub providers: HashMap<String, ModelProviderConfig>,
    pub models: HashMap<String, ModelGroupConfig>,
}

impl ModelRegistryConfig {
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

        let inference = InferenceConfig::default();
        let models = build_default_model_groups(&providers, &inference);

        Self { providers, models }
    }

    pub fn merge_with_auto_discovered(&mut self) {
        let discovered = Self::auto_discover();
        for (name, provider) in discovered.providers {
            self.providers.entry(name).or_insert(provider);
        }
    }

    pub fn parse_model_groups(&self, inference: &InferenceConfig) -> Result<HashMap<String, ModelGroup>, InferenceError> {
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
                    name: name.clone(),
                    main,
                    fallbacks,
                    max_tokens: config.max_tokens,
                    temperature: config.temperature,
                    context_window: config.context_window,
                    retry: config.retry.clone(),
                    inference: inference.clone(),
                },
            );
        }

        Ok(groups)
    }
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
    inference: &InferenceConfig,
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
                max_tokens: Some(inference.default_max_tokens),
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
