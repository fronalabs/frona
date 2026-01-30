use std::collections::HashMap;
use std::sync::Arc;

use rig::client::Nothing;
use rig::providers::{
    anthropic, cohere, deepseek, galadriel, gemini, groq, huggingface, hyperbolic, mira, mistral,
    moonshot, ollama, openai, openrouter, perplexity, together, xai,
};

use super::config::{ModelGroup, ModelRegistryConfig, ModelProviderConfig};
use super::error::LlmError;
use super::provider::{ModelProvider, RigProvider};

#[derive(Clone)]
pub struct ModelProviderRegistry {
    providers: Arc<HashMap<String, Arc<dyn ModelProvider>>>,
    model_groups: Arc<HashMap<String, ModelGroup>>,
}

impl ModelProviderRegistry {
    pub fn from_config(config: ModelRegistryConfig) -> Result<Self, LlmError> {
        let model_groups = config.parse_model_groups()?;
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();

        for (name, entry) in &config.providers {
            if !entry.enabled {
                tracing::info!(provider = %name, "Provider disabled, skipping");
                continue;
            }

            match init_provider(name, entry) {
                Ok(provider) => {
                    tracing::info!(provider = %name, "Provider initialized");
                    providers.insert(name.clone(), provider);
                }
                Err(e) => {
                    tracing::warn!(provider = %name, error = %e, "Failed to initialize provider");
                }
            }
        }

        if providers.is_empty() {
            tracing::warn!("No LLM providers configured — chat will fail until a provider is available");
        }

        Ok(Self {
            providers: Arc::new(providers),
            model_groups: Arc::new(model_groups),
        })
    }

    pub fn get_provider(&self, name: &str) -> Result<&dyn ModelProvider, LlmError> {
        self.providers
            .get(name)
            .map(|p| p.as_ref())
            .ok_or_else(|| LlmError::ProviderNotConfigured(name.to_string()))
    }

    pub fn get_model_group(&self, group_name: &str) -> Result<&ModelGroup, LlmError> {
        self.model_groups
            .get(group_name)
            .ok_or_else(|| LlmError::ModelGroupNotFound(group_name.to_string()))
    }

    pub fn has_model_group(&self, group_name: &str) -> bool {
        self.model_groups.contains_key(group_name)
    }
}

macro_rules! init_api_key_provider {
    ($name:expr, $entry:expr, $mod:ident) => {{
        let key = require_api_key($name, $entry)?;
        let client: $mod::Client = if let Some(url) = &$entry.base_url {
            $mod::Client::builder()
                .api_key(&key)
                .base_url(url)
                .build()
                .map_err(|e| LlmError::ConfigError(format!("{}: {e}", $name)))?
        } else {
            $mod::Client::new(&key)
                .map_err(|e| LlmError::ConfigError(format!("{}: {e}", $name)))?
        };
        Ok(Arc::new(RigProvider::new(client)) as Arc<dyn ModelProvider>)
    }};
}

fn init_provider(
    name: &str,
    entry: &ModelProviderConfig,
) -> Result<Arc<dyn ModelProvider>, LlmError> {
    match name {
        "openai" => init_api_key_provider!(name, entry, openai),
        "anthropic" => {
            let key = require_api_key(name, entry)?;
            let client: anthropic::Client = if let Some(url) = &entry.base_url {
                anthropic::Client::builder()
                    .api_key(&key)
                    .base_url(url)
                    .build()
                    .map_err(|e| LlmError::ConfigError(format!("anthropic: {e}")))?
            } else {
                anthropic::Client::builder()
                    .api_key(&key)
                    .build()
                    .map_err(|e| LlmError::ConfigError(format!("anthropic: {e}")))?
            };
            Ok(Arc::new(RigProvider::new(client)))
        }
        "ollama" => {
            let client: ollama::Client = if let Some(url) = &entry.base_url {
                ollama::Client::builder()
                    .api_key(Nothing)
                    .base_url(url)
                    .build()
                    .map_err(|e| LlmError::ConfigError(format!("ollama: {e}")))?
            } else {
                ollama::Client::new(Nothing)
                    .map_err(|e| LlmError::ConfigError(format!("ollama: {e}")))?
            };
            Ok(Arc::new(RigProvider::new(client)))
        }
        "groq" => init_api_key_provider!(name, entry, groq),
        "openrouter" => init_api_key_provider!(name, entry, openrouter),
        "deepseek" => init_api_key_provider!(name, entry, deepseek),
        "gemini" => init_api_key_provider!(name, entry, gemini),
        "cohere" => {
            let key = require_api_key(name, entry)?;
            let client: cohere::Client = if let Some(url) = &entry.base_url {
                cohere::Client::builder()
                    .api_key(&key)
                    .base_url(url)
                    .build()
                    .map_err(|e| LlmError::ConfigError(format!("cohere: {e}")))?
            } else {
                cohere::Client::new(&key)
                    .map_err(|e| LlmError::ConfigError(format!("cohere: {e}")))?
            };
            Ok(Arc::new(RigProvider::new(client)))
        }
        "mistral" => init_api_key_provider!(name, entry, mistral),
        "perplexity" => init_api_key_provider!(name, entry, perplexity),
        "together" => init_api_key_provider!(name, entry, together),
        "xai" => init_api_key_provider!(name, entry, xai),
        "hyperbolic" => init_api_key_provider!(name, entry, hyperbolic),
        "moonshot" => init_api_key_provider!(name, entry, moonshot),
        "mira" => init_api_key_provider!(name, entry, mira),
        "galadriel" => {
            let key = require_api_key(name, entry)?;
            let client: galadriel::Client = if let Some(url) = &entry.base_url {
                galadriel::Client::builder()
                    .api_key(&key)
                    .base_url(url)
                    .build()
                    .map_err(|e| LlmError::ConfigError(format!("galadriel: {e}")))?
            } else {
                galadriel::Client::builder()
                    .api_key(&key)
                    .build()
                    .map_err(|e| LlmError::ConfigError(format!("galadriel: {e}")))?
            };
            Ok(Arc::new(RigProvider::new(client)))
        }
        "huggingface" => init_api_key_provider!(name, entry, huggingface),
        _ => Err(LlmError::ProviderNotConfigured(format!(
            "Unknown provider: {name}"
        ))),
    }
}

fn require_api_key(provider: &str, entry: &ModelProviderConfig) -> Result<String, LlmError> {
    entry.api_key.clone().ok_or_else(|| {
        LlmError::ConfigError(format!(
            "Provider '{provider}' requires an api_key but none was provided"
        ))
    })
}
