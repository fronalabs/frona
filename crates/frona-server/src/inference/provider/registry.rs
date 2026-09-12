use std::{collections::HashMap, sync::Arc};

use crate::inference::{
    error::InferenceError,
    provider::{ModelProvider, ModelRef, group::ModelGroup},
};

#[derive(Clone)]
pub(crate) struct ModelProviderRegistry {
    providers: Arc<HashMap<String, Arc<dyn ModelProvider>>>,
    model_groups: Arc<HashMap<String, ModelGroup>>,
}

impl ModelProviderRegistry {
    pub(crate) fn new(
        providers: HashMap<String, Arc<dyn ModelProvider>>,
        groups: HashMap<String, ModelGroup>,
    ) -> Self {
        let providers = Arc::new(providers);
        Self {
            providers,
            model_groups: Arc::new(groups),
        }
    }

    pub(crate) fn providers(&self) -> &Arc<HashMap<String, Arc<dyn ModelProvider>>> {
        &self.providers
    }

    pub fn resolve(&self, reference: &ModelRef) -> Result<ModelGroup, InferenceError> {
        if reference.as_str().is_empty() {
            return Err(InferenceError::ModelGroupNotFound(
                reference.as_str().into(),
            ));
        }
        self.model_groups
            .get(reference.as_str())
            .cloned()
            .ok_or_else(|| InferenceError::ModelGroupNotFound(reference.as_str().into()))
    }

    pub fn resolve_with_fallback(
        &self,
        reference: &ModelRef,
        fallback: &ModelRef,
    ) -> Result<ModelGroup, InferenceError> {
        match self.resolve(reference) {
            Err(error) if !reference.as_str().is_empty() => {
                self.resolve(fallback).map_err(|_| error)
            }
            result => result,
        }
    }

    pub async fn unavailable_models(&self) -> HashMap<String, Vec<(String, String)>> {
        let mut unavailable = HashMap::new();
        for (name, group) in self.model_groups.iter() {
            let mut reasons = Vec::new();
            for model in std::iter::once(&group.main).chain(&group.fallbacks) {
                if let Err(error) = async {
                    let provider = self.providers.get(model.provider_name()).ok_or_else(|| {
                        InferenceError::ProviderNotConfigured(model.provider_name().into())
                    })?;
                    provider.ensure_usable(model).await
                }
                .await
                {
                    reasons.push((model.as_str(), error.to_string()));
                }
            }
            if !reasons.is_empty() {
                unavailable.insert(name.clone(), reasons);
            }
        }
        unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_groups_and_lookup_only_copies_the_selected_group() {
        let config: crate::core::config::Config = serde_json::from_value(serde_json::json!({
            "providers":{"work":{"provider":"openai"}},
            "models":{
                "primary":{"provider":"work","model":"first","extra_params":{"nested":{"values":[1,2,3]}},
                    "fallbacks":[{"provider":"work","model":"backup"}]},
                "title":{"provider":"work","model":"second"}
            }
        })).unwrap();
        let groups = crate::inference::config::ModelRegistryConfig {
            providers: config.providers,
            models: config.models,
            skip_auto_discover: true,
        }
        .parse_model_groups(&config.inference, Default::default())
        .unwrap();
        let registry = ModelProviderRegistry::new(HashMap::new(), groups);
        let clone = registry.clone();
        assert!(Arc::ptr_eq(&registry.model_groups, &clone.model_groups));
        assert!(Arc::ptr_eq(&registry.providers, &clone.providers));
        let mut selected = clone.resolve(&ModelRef::PRIMARY).unwrap();
        selected.main.request_settings.extra_params.clear();
        selected.fallbacks.clear();
        let original = registry.resolve(&ModelRef::PRIMARY).unwrap();
        assert_eq!(original.fallbacks.len(), 1);
        assert!(
            original
                .main
                .request_settings
                .extra_params
                .contains_key("nested")
        );
        assert_eq!(
            registry.resolve(&ModelRef::TITLE).unwrap().main.model_id,
            "second"
        );
    }
}
