use crate::core::{Handle, config::ModelProviderConfig};
use frona_harness::{
    config::{InferenceConfig, ModelGroupConfig},
    inference::{InferenceError, ModelGroup, provider::ModelProvider},
};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug)]
pub struct ModelRegistryConfig {
    pub providers: HashMap<Handle, ModelProviderConfig>,
    pub models: HashMap<String, ModelGroupConfig>,
    pub skip_auto_discover: bool,
}
impl ModelRegistryConfig {
    fn portable(
        &self,
    ) -> Result<frona_harness::inference::config::ModelRegistryConfig, InferenceError> {
        for group in self.models.values() {
            for model in std::iter::once(group).chain(&group.common.fallbacks) {
                if let Some(config) = self.providers.get(model.provider.as_str()) {
                    crate::inference::provider::resolve_connection(
                        &model.provider.clone().into(),
                        config,
                    )?;
                }
            }
        }
        Ok(frona_harness::inference::config::ModelRegistryConfig {
            providers: self
                .providers
                .iter()
                .map(|(handle, config)| (handle.clone().into(), config.into()))
                .collect(),
            models: self.models.clone(),
            skip_auto_discover: self.skip_auto_discover,
        })
    }

    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
            models: HashMap::new(),
            skip_auto_discover: true,
        }
    }
    pub fn auto_discover() -> Self {
        let config = frona_harness::inference::config::ModelRegistryConfig::auto_discover();
        Self {
            providers: config
                .providers
                .into_iter()
                .map(|(handle, config)| (handle.into(), config.into()))
                .collect(),
            models: config.models,
            skip_auto_discover: config.skip_auto_discover,
        }
    }
    pub fn merge_with_auto_discovered(&mut self) {
        if !self.skip_auto_discover {
            for (handle, config) in Self::auto_discover().providers {
                self.providers.entry(handle).or_insert(config);
            }
        }
    }
    pub fn validate_model_groups(&self) -> Result<(), InferenceError> {
        self.portable()?.validate_model_groups()
    }
    pub fn parse_model_groups(
        &self,
        inference: &InferenceConfig,
        providers: Arc<HashMap<String, Arc<dyn ModelProvider>>>,
    ) -> Result<HashMap<String, ModelGroup>, InferenceError> {
        self.portable()?.parse_model_groups(inference, providers)
    }
    pub fn parse_model_groups_with_catalog(
        &self,
        inference: &InferenceConfig,
        catalog: &frona_model_catalog::ModelCatalogSnapshot,
        providers: Arc<HashMap<String, Arc<dyn ModelProvider>>>,
    ) -> Result<HashMap<String, ModelGroup>, InferenceError> {
        self.portable()?
            .parse_model_groups_with_catalog(inference, catalog, providers)
    }
    pub fn parameter_overrides(
        &self,
    ) -> Result<
        Vec<frona_harness::inference::protocol::parameters::ParameterOverride>,
        InferenceError,
    > {
        self.portable()?.parameter_overrides()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frona_harness::config::OpenAiApi;
    use frona_harness::config::ProviderModel;

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
