use std::{collections::HashMap, sync::Arc};

use serde::de::DeserializeOwned;

use crate::core::Handle;
pub use crate::core::config::{
    ApiSurface, CommonModelFields, InferenceConfig, ModelGroupConfig, ModelProviderConfig,
    ProviderModel, RetryConfig,
};

use crate::inference::error::InferenceError;
use crate::inference::{
    ModelGroup,
    provider::{ModelConfig, ModelProvider},
};

#[derive(Debug)]
pub struct ModelRegistryConfig {
    pub providers: HashMap<Handle, ModelProviderConfig>,
    pub models: HashMap<String, ModelGroupConfig>,
    pub skip_auto_discover: bool,
}

fn typed_params<T: DeserializeOwned>(
    path: &str,
    adapter: &str,
    config: &ModelGroupConfig,
    allowed: &[&str],
) -> Result<T, InferenceError> {
    let mut value = serde_json::to_value(&config.settings).expect("model settings serialize");
    let object = value.as_object_mut().expect("model settings are an object");
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(InferenceError::ConfigError(format!(
            "{path}.{key}: setting is not supported by adapter '{adapter}'"
        )));
    }
    serde_json::from_value(value).map_err(|error| {
        InferenceError::ConfigError(format!("{path}: invalid settings for '{adapter}': {error}"))
    })
}

pub(crate) fn compile_request(
    path: &str,
    config: &ModelGroupConfig,
    connection: &ModelProviderConfig,
) -> Result<ProviderModel, InferenceError> {
    use crate::core::config::{AnthropicParams, GeminiParams, OllamaParams};

    let resolved = crate::inference::provider::platform::ProviderPlatform::resolve(
        &config.provider,
        connection,
    )?;
    if resolved.factory == crate::inference::provider::platform::FactoryKind::Azure {
        crate::inference::provider::adapter::azure::validate_deployment(&config.common.model)?;
    }
    let adapter = resolved.request_adapter_name();
    let surface = config.api.unwrap_or(resolved.protocols[0]);
    if !resolved.supports_model_protocol(&config.common.model, surface) {
        return Err(unsupported_surface(path, adapter));
    }
    match adapter {
        "bedrock" => Ok(ProviderModel::Bedrock {
            params: typed_params::<crate::core::config::BedrockParams>(
                path,
                adapter,
                config,
                &["top_p", "stop_sequences"],
            )?,
        }),
        "anthropic" => {
            if surface != ApiSurface::AnthropicMessages {
                return Err(unsupported_surface(path, adapter));
            }
            Ok(ProviderModel::Anthropic {
                params: typed_params::<AnthropicParams>(
                    path,
                    adapter,
                    config,
                    &["thinking", "top_p", "top_k", "stop_sequences"],
                )?,
            })
        }
        "ollama" => {
            if surface != ApiSurface::Ollama {
                return Err(unsupported_surface(path, adapter));
            }
            Ok(ProviderModel::Ollama {
                params: typed_params::<OllamaParams>(
                    path,
                    adapter,
                    config,
                    &[
                        "think",
                        "num_ctx",
                        "num_predict",
                        "num_batch",
                        "num_keep",
                        "num_thread",
                        "num_gpu",
                        "top_k",
                        "top_p",
                        "min_p",
                        "repeat_penalty",
                        "repeat_last_n",
                        "frequency_penalty",
                        "presence_penalty",
                        "mirostat",
                        "mirostat_eta",
                        "mirostat_tau",
                        "tfs_z",
                        "seed",
                        "stop",
                        "use_mmap",
                        "use_mlock",
                    ],
                )?,
            })
        }
        "gemini" => {
            if surface != ApiSurface::GoogleGenerateContent {
                return Err(unsupported_surface(path, adapter));
            }
            Ok(ProviderModel::Gemini {
                params: typed_params::<GeminiParams>(
                    path,
                    adapter,
                    config,
                    &[
                        "thinking_config",
                        "top_p",
                        "top_k",
                        "stop_sequences",
                        "candidate_count",
                    ],
                )?,
            })
        }
        "openai" => {
            let api = crate::core::config::OpenAiApi::try_from(surface)
                .map_err(|message| InferenceError::ConfigError(format!("{path}.api: {message}")))?;
            Ok(ProviderModel::OpenAI {
                api: Some(api),
                params: openai_params(path, adapter, config)?,
            })
        }
        name @ ("groq" | "openrouter" | "deepseek" | "xai" | "together" | "hyperbolic") => {
            if surface != ApiSurface::Completions {
                return Err(unsupported_surface(path, adapter));
            }
            let params = openai_params(path, adapter, config)?;
            Ok(match name {
                "groq" => ProviderModel::Groq { params },
                "openrouter" => ProviderModel::OpenRouter { params },
                "deepseek" => ProviderModel::DeepSeek { params },
                "xai" => ProviderModel::XAI { params },
                "together" => ProviderModel::Together { params },
                _ => ProviderModel::Hyperbolic { params },
            })
        }
        "generic" => {
            let params = openai_params(path, adapter, config)?;
            if surface != ApiSurface::Completions {
                return Err(unsupported_surface(path, adapter));
            }
            Ok(ProviderModel::OpenAI {
                api: Some(crate::core::config::OpenAiApi::ChatCompletions),
                params,
            })
        }
        other => {
            if !config.settings_is_empty() {
                let settings = serde_json::to_value(&config.settings).expect("settings serialize");
                let field = settings
                    .as_object()
                    .expect("settings object")
                    .keys()
                    .next()
                    .expect("nonempty settings");
                return Err(InferenceError::ConfigError(format!(
                    "{path}.{field}: setting is not supported by adapter '{other}'"
                )));
            }
            Ok(ProviderModel::Custom {
                name: other.to_string(),
            })
        }
    }
}

fn unsupported_surface(path: &str, adapter: &str) -> InferenceError {
    InferenceError::ConfigError(format!(
        "{path}.api: protocol is not supported by adapter '{adapter}'"
    ))
}

fn openai_params(
    path: &str,
    adapter: &str,
    config: &ModelGroupConfig,
) -> Result<crate::core::config::OpenAICompatParams, InferenceError> {
    typed_params(
        path,
        adapter,
        config,
        &[
            "top_p",
            "min_p",
            "frequency_penalty",
            "presence_penalty",
            "seed",
            "max_completion_tokens",
            "reasoning_effort",
            "logprobs",
            "top_logprobs",
            "stop",
        ],
    )
}

impl ModelGroupConfig {
    fn settings_is_empty(&self) -> bool {
        serde_json::to_value(&self.settings)
            .expect("model settings serialize")
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
    }
}

impl ModelRegistryConfig {
    /// Validate authoring input without creating runtime provider clients.
    pub fn validate_model_groups(&self) -> Result<(), InferenceError> {
        for (name, config) in &self.models {
            self.compile_ref(&format!("models.{name}"), config)?;
            for (index, fallback) in config.common.fallbacks.iter().enumerate() {
                self.compile_ref(&format!("models.{name}.fallbacks.{index}"), fallback)?;
            }
        }
        Ok(())
    }

    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
            models: HashMap::new(),
            skip_auto_discover: true,
        }
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
            if std::env::var(env_var).is_ok() {
                providers.insert(
                    Handle::try_new(name).expect("built-in provider handle"),
                    ModelProviderConfig {
                        api_key: Some(format!("${{{env_var}}}")),
                        ..Default::default()
                    },
                );
            }
        }
        if let Ok(url) = std::env::var("OLLAMA_API_BASE_URL") {
            providers.insert(
                Handle::const_validated("ollama"),
                ModelProviderConfig {
                    base_url: Some(url),
                    ..Default::default()
                },
            );
        }
        let inference = InferenceConfig::default();
        let models = build_default_model_groups(&providers, &inference);
        Self {
            providers,
            models,
            skip_auto_discover: false,
        }
    }

    pub fn merge_with_auto_discovered(&mut self) {
        if self.skip_auto_discover {
            return;
        }
        for (handle, provider) in Self::auto_discover().providers {
            self.providers.entry(handle).or_insert(provider);
        }
    }

    pub fn parse_model_groups(
        &self,
        inference: &InferenceConfig,
        providers: Arc<HashMap<String, Arc<dyn ModelProvider>>>,
    ) -> Result<HashMap<String, ModelGroup>, InferenceError> {
        self.parse_model_groups_with_catalog(
            inference,
            &frona_model_catalog::ModelCatalogSnapshot::empty(),
            providers,
        )
    }

    /// Resolve budgeting from an optional local snapshot without changing
    /// protocol selection or materializing inferred settings in configuration.
    pub fn parse_model_groups_with_catalog(
        &self,
        inference: &InferenceConfig,
        catalog: &frona_model_catalog::ModelCatalogSnapshot,
        providers: Arc<HashMap<String, Arc<dyn ModelProvider>>>,
    ) -> Result<HashMap<String, ModelGroup>, InferenceError> {
        let mut groups = HashMap::new();
        for (name, config) in &self.models {
            let main = self.compile_ref(&format!("models.{name}"), config)?;
            let fallbacks = config
                .common
                .fallbacks
                .iter()
                .enumerate()
                .map(|(index, fallback)| {
                    self.compile_ref(&format!("models.{name}.fallbacks.{index}"), fallback)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let context_window = match config.common.context_window {
                Some(window) => window,
                None => {
                    let connection =
                        crate::inference::provider::platform::ProviderPlatform::resolve(
                            &config.provider,
                            &self.providers[&config.provider],
                        )?;
                    catalog
                        .lookup_for_provider(&connection.brand, &config.common.model)
                        .and_then(|entry| entry.max_input_tokens())
                        .and_then(|limit| usize::try_from(limit).ok())
                        .unwrap_or(crate::inference::context::DEFAULT_CONTEXT_WINDOW)
                }
            };
            groups.insert(
                name.clone(),
                ModelGroup {
                    providers: providers.clone(),
                    name: name.clone(),
                    main,
                    fallbacks,
                    max_tokens: config.common.max_tokens,
                    temperature: config.common.temperature,
                    context_window,
                    retry: config.common.retry.clone(),
                    inference: inference.clone(),
                },
            );
        }
        Ok(groups)
    }

    fn compile_ref(
        &self,
        path: &str,
        config: &ModelGroupConfig,
    ) -> Result<ModelConfig, InferenceError> {
        let connection = self.providers.get(&config.provider).ok_or_else(|| {
            InferenceError::ConfigError(format!(
                "{path}.provider: provider '{}' is not configured",
                config.provider
            ))
        })?;
        let model = ModelConfig {
            catalog_provider: crate::inference::provider::platform::ProviderPlatform::resolve(
                &config.provider,
                connection,
            )?
            .brand,
            request_settings: crate::inference::provider::ModelRequestSettings {
                max_tokens: config.common.max_tokens,
                temperature: config.common.temperature,
                extra_params: config.common.extra_params.clone(),
            },
            provider_handle: config.provider.clone(),
            model_id: config.common.model.clone(),
            provider: compile_request(path, config, connection)?,
        };
        crate::inference::protocol::parameters::WireParameters::prepare_named(
            &model, None, None, false, path,
        )?;
        Ok(model)
    }

    pub fn parameter_overrides(
        &self,
    ) -> Result<Vec<crate::inference::protocol::parameters::ParameterOverride>, InferenceError>
    {
        let mut warnings = Vec::new();
        for (name, group) in &self.models {
            for (path, config) in std::iter::once((format!("models.{name}"), group)).chain(
                group
                    .common
                    .fallbacks
                    .iter()
                    .enumerate()
                    .map(|(index, fallback)| {
                        (format!("models.{name}.fallbacks.{index}"), fallback)
                    }),
            ) {
                let model = self.compile_ref(&path, config)?;
                warnings.extend(
                    crate::inference::protocol::parameters::WireParameters::prepare_named(
                        &model, None, None, false, &path,
                    )?
                    .overrides,
                );
            }
        }
        warnings.sort_by(|a, b| a.config_path.cmp(&b.config_path));
        Ok(warnings)
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

fn build_default_model_config(provider: &Handle, model: &str, max_tokens: u64) -> ModelGroupConfig {
    ModelGroupConfig {
        provider: provider.clone(),
        api: None,
        common: CommonModelFields {
            model: model.to_string(),
            max_tokens: Some(max_tokens),
            ..Default::default()
        },
        settings: Default::default(),
    }
}

fn build_default_model_groups(
    providers: &HashMap<Handle, ModelProviderConfig>,
    inference: &InferenceConfig,
) -> HashMap<String, ModelGroupConfig> {
    let mut models = HashMap::new();
    if let Some((provider, _)) = providers.iter().next() {
        models.insert(
            "primary".to_string(),
            build_default_model_config(
                provider,
                default_model_for_provider(provider.as_str()),
                inference.default_max_tokens,
            ),
        );
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::OpenAiApi;

    fn registry(yaml: &str) -> ModelRegistryConfig {
        let config: crate::core::config::Config = serde_yaml::from_str(yaml).unwrap();
        ModelRegistryConfig {
            providers: config.providers,
            models: config.models,
            skip_auto_discover: true,
        }
    }

    #[test]
    fn named_handle_and_protocol_are_independent() {
        let registry = registry(
            "providers:\n  openai-production:\n    provider: openai\nmodels:\n  primary:\n    provider: openai-production\n    model: gpt-test\n    api: responses\n    reasoning_effort: high\n",
        );
        let groups = registry
            .parse_model_groups(&InferenceConfig::default(), Default::default())
            .unwrap();
        assert_eq!(groups["primary"].main.provider_name(), "openai-production");
        let ProviderModel::OpenAI { api, params } = &groups["primary"].main.provider else {
            panic!("expected OpenAI request");
        };
        assert_eq!(*api, Some(OpenAiApi::Responses));
        assert_eq!(params.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn openai_defaults_to_responses_without_catalogs() {
        let registry = registry(
            "providers:\n  openai:\n    api_key: literal\nmodels:\n  primary:\n    provider: openai\n    model: unknown\n",
        );
        let groups = registry
            .parse_model_groups(&InferenceConfig::default(), Default::default())
            .unwrap();
        let ProviderModel::OpenAI { api, .. } = &groups["primary"].main.provider else {
            panic!("expected OpenAI request");
        };
        assert_eq!(*api, Some(OpenAiApi::Responses));
    }

    #[test]
    fn compatible_providers_and_explicit_chat_completions_keep_their_protocol() {
        for (connection, api) in [
            ("provider: openai", "api: completions"),
            (
                "provider: custom\n    adapter: openai\n    base_url: https://example.com/v1",
                "",
            ),
        ] {
            let groups = registry(&format!(
                "providers:\n  account:\n    {connection}\nmodels:\n  primary:\n    provider: account\n    model: gpt-5.6-luna\n    {api}\n"
            ))
            .parse_model_groups(&InferenceConfig::default(), Default::default())
            .unwrap();
            assert!(matches!(
                groups["primary"].main.provider,
                ProviderModel::OpenAI {
                    api: Some(OpenAiApi::ChatCompletions),
                    ..
                }
            ));
        }
    }

    #[test]
    fn missing_provider_and_foreign_fallback_setting_have_paths() {
        let missing = registry("models:\n  primary:\n    provider: absent\n    model: x\n")
            .parse_model_groups(&InferenceConfig::default(), Default::default())
            .unwrap_err()
            .to_string();
        assert!(missing.contains("models.primary.provider"), "{missing}");

        let foreign = registry(
            "providers:\n  anthropic: {}\nmodels:\n  primary:\n    provider: anthropic\n    model: x\n    fallbacks:\n      - provider: anthropic\n        model: y\n        reasoning_effort: high\n",
        )
        .parse_model_groups(&InferenceConfig::default(), Default::default())
        .unwrap_err()
        .to_string();
        assert!(
            foreign.contains("models.primary.fallbacks.0.reasoning_effort"),
            "{foreign}"
        );
    }

    #[test]
    fn nested_extra_params_round_trip() {
        let input = "provider: openai-production\nmodel: x\napi: responses\nextra_params:\n  reasoning:\n    summary: detailed\n  explicit: null\n";
        let config: ModelGroupConfig = serde_yaml::from_str(input).unwrap();
        let output = serde_yaml::to_string(&config).unwrap();
        let again: ModelGroupConfig = serde_yaml::from_str(&output).unwrap();
        assert_eq!(again.common.extra_params, config.common.extra_params);
        assert_eq!(again.provider.as_str(), "openai-production");
    }

    #[test]
    fn main_and_fallback_keep_independent_model_settings() {
        let input = "providers:\n  account:\n    provider: openai\nmodels:\n  primary:\n    provider: account\n    model: main\n    api: responses\n    temperature: 0.7\n    context_window: 4096\n    fallbacks:\n      - provider: account\n        model: backup\n        api: completions\n        temperature: 0.3\n";
        let config: crate::core::config::Config = serde_yaml::from_str(input).unwrap();
        let round_trip = serde_yaml::to_string(&config).unwrap();
        let groups = registry(&round_trip)
            .parse_model_groups(&InferenceConfig::default(), Default::default())
            .unwrap();
        let group = &groups["primary"];
        assert_eq!(group.context_window, 4096);
        assert_eq!(group.main.model_id, "main");
        assert_eq!(group.main.request_settings.temperature, Some(0.7));
        assert_eq!(group.fallbacks[0].model_id, "backup");
        assert_eq!(group.fallbacks[0].request_settings.temperature, Some(0.3));
        assert!(matches!(
            group.main.provider,
            ProviderModel::OpenAI {
                api: Some(OpenAiApi::Responses),
                ..
            }
        ));
        assert!(matches!(
            group.fallbacks[0].provider,
            ProviderModel::OpenAI {
                api: Some(OpenAiApi::ChatCompletions),
                ..
            }
        ));
    }

    #[test]
    fn unknown_top_level_setting_is_rejected() {
        let error = serde_yaml::from_str::<ModelGroupConfig>(
            "provider: openai\nmodel: x\ntemprature: 0.2\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("temprature"), "{error}");
    }

    #[test]
    fn provider_handle_normalization_rejects_collisions() {
        let error = serde_yaml::from_str::<crate::core::config::Config>(
            "providers:\n  OpenAI: {}\n  ' openai ': {}\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("collides after trimming and lowercasing"),
            "{error}"
        );
    }
}
