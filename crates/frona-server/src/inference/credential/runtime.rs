use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

use crate::core::{
    Handle,
    config::{ApiSurface, ModelProviderConfig, ProviderModel},
};
use crate::inference::credential::store::{CredentialMethod, ProviderCredentials};
use crate::inference::{
    error::InferenceError,
    provider::{
        InferenceCounter, ModelConfig, ModelProvider,
        platform::{ProviderPlatform, build_provider},
    },
};

type EnvironmentLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

pub struct RuntimeCredentials {
    entries: HashMap<Handle, Arc<PreparedProvider>>,
    store: ProviderCredentials,
    counter: InferenceCounter,
    #[cfg(test)]
    method_drivers: Vec<Arc<dyn CredentialMethodDriver>>,
}

/// Compiled authentication implementations register through the provider platform.
/// A driver must check its trusted routing before accepting a connection.
#[async_trait::async_trait]
pub trait CredentialMethodDriver: Send + Sync {
    fn method(&self) -> CredentialMethod;

    fn accepts(
        &self,
        connection: &crate::inference::provider::platform::ResolvedConnection,
    ) -> bool;

    fn protocols(&self) -> &[ApiSurface];

    fn build(
        &self,
        connection: &crate::inference::provider::platform::ResolvedConnection,
        config: &ModelProviderConfig,
        secret: &crate::credential::managed::integration::ErasedSecret,
        counter: &InferenceCounter,
    ) -> Result<Arc<dyn ModelProvider>, InferenceError>;
}

pub struct PreparedProvider {
    config: ModelProviderConfig,
    connection: Result<crate::inference::provider::platform::ResolvedConnection, String>,
    cache: Mutex<Option<CachedClient>>,
    drivers: Vec<Arc<dyn CredentialMethodDriver>>,
    store: ProviderCredentials,
    counter: InferenceCounter,
    environment: Arc<EnvironmentLookup>,
}

struct CachedClient {
    fingerprint: [u8; 32],
    client: Arc<dyn ModelProvider>,
}

#[derive(Clone)]
pub(crate) struct ResolvedProvider {
    pub client: Arc<dyn ModelProvider>,
    pub identity: [u8; 32],
    pub method: CredentialMethod,
    pub protocols: Vec<ApiSurface>,
}

#[derive(Clone, serde::Serialize)]
pub struct EffectiveAuthentication {
    pub method: Option<CredentialMethod>,
    pub source: String,
}

impl RuntimeCredentials {
    /// Non-secret selection metadata. This follows the same priority as resolve;
    /// per-model protocol eligibility is reported separately by the registry.
    pub async fn effective_authentication(
        &self,
        handle: &Handle,
    ) -> Result<EffectiveAuthentication, crate::core::error::AppError> {
        use crate::core::error::AppError;
        let entry = self
            .entries
            .get(handle)
            .ok_or_else(|| AppError::NotFound("provider is not active in this runtime".into()))?;
        entry
            .connection
            .as_ref()
            .map_err(|error| AppError::Validation(error.clone()))?;
        if let Some(variable) = entry
            .config
            .api_key
            .as_deref()
            .and_then(environment_reference)
        {
            return Ok(EffectiveAuthentication {
                method: Some(CredentialMethod::ApiKey),
                source: format!("environment:{variable}"),
            });
        }
        if let Some(id) = entry.config.credential_id {
            let status = self
                .store
                .vault()
                .status_by_id(id)
                .await?
                .filter(|status| !status.removed)
                .ok_or_else(|| AppError::NotFound("managed credential".into()))?;
            let method = match status.integration.as_str() {
                "static" => CredentialMethod::ApiKey,
                "copilot" | "openai_codex" => CredentialMethod::Oauth,
                _ => {
                    return Err(AppError::Validation(
                        "unsupported credential integration".into(),
                    ));
                }
            };
            return Ok(EffectiveAuthentication {
                method: Some(method),
                source: "managed".into(),
            });
        }
        if entry
            .config
            .api_key
            .as_ref()
            .is_some_and(|key| !key.is_empty())
        {
            return Ok(EffectiveAuthentication {
                method: Some(CredentialMethod::ApiKey),
                source: "inline".into(),
            });
        }
        let resolved = entry
            .connection
            .as_ref()
            .map_err(|error| AppError::Validation(error.clone()))?;
        if resolved
            .auth_methods
            .iter()
            .any(|method| method.method == CredentialMethod::Anonymous)
        {
            return Ok(EffectiveAuthentication {
                method: Some(CredentialMethod::Anonymous),
                source: "anonymous".into(),
            });
        }
        if resolved
            .auth_methods
            .iter()
            .any(|method| method.method == CredentialMethod::Aws)
        {
            return Ok(EffectiveAuthentication {
                method: Some(CredentialMethod::Aws),
                source: "ambient".into(),
            });
        }
        Ok(EffectiveAuthentication {
            method: None,
            source: "unavailable".into(),
        })
    }

    #[cfg(test)]
    pub fn with_method_driver(mut self, driver: Arc<dyn CredentialMethodDriver>) -> Self {
        self.method_drivers.push(driver.clone());
        for entry in self.entries.values_mut() {
            Arc::get_mut(entry)
                .expect("drivers are registered before sharing providers")
                .drivers
                .insert(0, driver.clone());
        }
        self
    }

    pub fn new(
        configs: HashMap<Handle, ModelProviderConfig>,
        store: ProviderCredentials,
        counter: InferenceCounter,
    ) -> Self {
        Self {
            entries: configs
                .into_iter()
                .map(|(handle, config)| {
                    (
                        handle.clone(),
                        Arc::new(PreparedProvider {
                            connection: ProviderPlatform::resolve(&handle, &config)
                                .map_err(|error| error.to_string()),
                            config,
                            cache: Mutex::new(None),
                            drivers: vec![
                                Arc::new(
                                    crate::inference::provider::adapter::copilot::Driver::default(),
                                ),
                                Arc::new(
                                    crate::inference::provider::adapter::chatgpt::Driver::default(),
                                ),
                            ],
                            store: store.clone(),
                            counter: counter.clone(),
                            environment: Arc::new(|name| std::env::var(name).ok()),
                        }),
                    )
                })
                .collect(),
            store,
            counter,
            #[cfg(test)]
            method_drivers: vec![],
        }
    }

    pub fn provider(&self, handle: &Handle) -> Result<Arc<PreparedProvider>, InferenceError> {
        self.entries
            .get(handle)
            .cloned()
            .ok_or_else(|| InferenceError::ProviderNotConfigured(handle.to_string()))
    }

    pub fn providers(&self) -> HashMap<String, Arc<dyn ModelProvider>> {
        self.entries
            .iter()
            .map(|(handle, provider)| {
                (
                    handle.to_string(),
                    provider.clone() as Arc<dyn ModelProvider>,
                )
            })
            .collect()
    }

    pub fn prepare_model(&self, model: &mut ModelConfig) -> Result<(), InferenceError> {
        let provider = self.provider(&model.provider_handle)?;
        let connection = provider
            .connection
            .as_ref()
            .map_err(|error| InferenceError::ConfigError(error.clone()))?;
        let adapter = connection.request_adapter_name();
        if model.provider.name() != adapter
            && !(model.provider.name() == "generic" && adapter == "openai")
        {
            return Err(InferenceError::ConfigError(
                "model settings do not match the configured provider adapter".into(),
            ));
        }
        if matches!(model.provider, ProviderModel::OpenAI { api: None, .. }) {
            return Err(InferenceError::ConfigError(
                "ad-hoc OpenAI models require an explicit API protocol".into(),
            ));
        }
        if !connection.supports_model_protocol(&model.model_id, model_protocol(model)) {
            return Err(InferenceError::ConfigError(
                "model protocol is not supported by the configured provider".into(),
            ));
        }
        if connection.factory == crate::inference::provider::platform::FactoryKind::Azure {
            crate::inference::provider::adapter::azure::validate_deployment(&model.model_id)?;
        }
        crate::inference::protocol::parameters::WireParameters::prepare_named(
            model, None, None, false, "model",
        )?;
        model.catalog_provider = connection.brand.clone();
        Ok(())
    }

    pub(crate) async fn resolve_for_listing(
        &self,
        handle: &Handle,
    ) -> Result<ResolvedProvider, InferenceError> {
        self.provider(handle)?
            .resolve_selected(
                &ModelConfig {
                    catalog_provider: String::new(),
                    provider_handle: handle.clone(),
                    model_id: "model-list".into(),
                    provider: ProviderModel::Generic,
                    request_settings: Default::default(),
                },
                false,
            )
            .await
    }

    pub(crate) async fn resolve_config_for_listing(
        &self,
        handle: &Handle,
        config: ModelProviderConfig,
    ) -> Result<ResolvedProvider, InferenceError> {
        let runtime = Self::new(
            HashMap::from([(handle.clone(), config)]),
            self.store.clone(),
            self.counter.clone(),
        );
        #[cfg(test)]
        let runtime = self.method_drivers.iter().fold(runtime, |runtime, driver| {
            runtime.with_method_driver(driver.clone())
        });
        runtime.resolve_for_listing(handle).await
    }
}

impl PreparedProvider {
    async fn resolve_selected(
        &self,
        model: &ModelConfig,
        require_protocol: bool,
    ) -> Result<ResolvedProvider, InferenceError> {
        let entry = self;
        if !entry.config.enabled {
            return Err(unavailable(model, "disabled"));
        }
        // Serialize selection and construction per handle. Each request checks the
        // current database generation; pending records never enter this path.
        let mut cache = entry.cache.lock().await;
        let resolved = self
            .connection
            .as_ref()
            .map_err(|error| InferenceError::ConfigError(error.clone()))?;
        if resolved.handle != model.provider_handle {
            return Err(unavailable(model, "provider handle mismatch"));
        }
        let mut effective = entry.config.clone();
        let mut managed_secret = None;
        let mut credential_identity = None;
        let method;
        if let Some(id) = entry.config.credential_id {
            let (status, secret) = self.store.resolve_credential(id).await.map_err(|_| {
                unavailable(
                    model,
                    "managed credential is missing, inaccessible, or unresolved",
                )
            })?;
            credential_identity = Some((status.item_id, status.version, secret.identity));
            match status.integration.as_str() {
                "static" => {
                    if !resolved
                        .auth_methods
                        .iter()
                        .any(|auth| auth.method == CredentialMethod::ApiKey)
                    {
                        return Err(unavailable(model, "credential integration is incompatible"));
                    }
                    effective.api_key = Some(
                        crate::credential::managed::integration::static_secret::api_key(
                            secret
                                .credentials()
                                .map_err(|_| unavailable(model, "invalid static credential"))?,
                        )
                        .map_err(|_| unavailable(model, "invalid API key credential"))?
                        .to_owned(),
                    );
                    method = CredentialMethod::ApiKey;
                }
                "copilot" => {
                    if resolved.factory
                        != crate::inference::provider::platform::FactoryKind::Copilot
                    {
                        return Err(unavailable(
                            model,
                            "Copilot credential requires the Copilot provider",
                        ));
                    }
                    method = CredentialMethod::Oauth;
                }
                "openai_codex" => {
                    let driver = self
                        .drivers
                        .iter()
                        .find(|driver| {
                            driver.method() == CredentialMethod::Oauth && driver.accepts(resolved)
                        })
                        .ok_or_else(|| {
                            unavailable(model, "credential integration is incompatible")
                        })?;
                    let protocols: Vec<_> = driver
                        .protocols()
                        .iter()
                        .copied()
                        .filter(|protocol| resolved.protocols.contains(protocol))
                        .collect();
                    if require_protocol && !protocols.contains(&model_protocol(model)) {
                        return Err(unavailable(model, "unsupported_protocol_for_auth_method"));
                    }
                    let fingerprint = *blake3::hash(
                        &serde_json::to_vec(&(credential_identity, &effective))
                            .expect("credential identity serializes"),
                    )
                    .as_bytes();
                    if let Some(hit) = cache.as_ref().filter(|hit| hit.fingerprint == fingerprint) {
                        return Ok(ResolvedProvider {
                            client: hit.client.clone(),
                            identity: fingerprint,
                            method: driver.method(),
                            protocols,
                        });
                    }
                    let client = driver.build(resolved, &effective, &secret, &self.counter)?;
                    *cache = Some(CachedClient {
                        fingerprint,
                        client: client.clone(),
                    });
                    return Ok(ResolvedProvider {
                        client,
                        identity: fingerprint,
                        method: driver.method(),
                        protocols,
                    });
                }
                _ => return Err(unavailable(model, "unsupported credential integration")),
            }
            managed_secret = Some(secret);
        } else if let Some(variable) = entry
            .config
            .api_key
            .as_deref()
            .and_then(environment_reference)
        {
            effective.api_key = Some(
                (self.environment)(variable)
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| {
                        unavailable(
                            model,
                            &format!("unresolved environment reference '{variable}'"),
                        )
                    })?,
            );
            method = CredentialMethod::ApiKey;
        } else if effective
            .api_key
            .as_ref()
            .is_some_and(|key| !key.is_empty())
        {
            method = CredentialMethod::ApiKey;
        } else if resolved
            .auth_methods
            .iter()
            .any(|auth| auth.method == CredentialMethod::Anonymous)
        {
            method = CredentialMethod::Anonymous;
        } else if resolved
            .auth_methods
            .iter()
            .any(|auth| auth.method == CredentialMethod::Aws)
        {
            method = CredentialMethod::Aws;
        } else {
            return Err(unavailable(model, "missing credential"));
        }
        let protocol = model_protocol(model);
        if require_protocol
            && (!resolved.protocols.contains(&protocol)
                || !resolved.auth_methods.iter().any(|descriptor| {
                    descriptor.method == method && descriptor.protocols.contains(&protocol)
                }))
        {
            return Err(unavailable(model, "unsupported_protocol_for_auth_method"));
        }
        let fingerprint = *blake3::hash(
            &serde_json::to_vec(&(credential_identity, method, &effective))
                .map_err(|error| InferenceError::ConfigError(error.to_string()))?,
        )
        .as_bytes();
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.fingerprint == fingerprint)
        {
            return Ok(ResolvedProvider {
                client: cached.client.clone(),
                identity: fingerprint,
                method,
                protocols: resolved
                    .auth_methods
                    .iter()
                    .filter(|entry| entry.method == method)
                    .flat_map(|entry| entry.protocols.iter().copied())
                    .collect(),
            });
        }
        let client = if let Some(secret) = managed_secret.as_ref().filter(|_| {
            resolved.factory == crate::inference::provider::platform::FactoryKind::Copilot
        }) {
            crate::inference::provider::adapter::copilot::current_provider(
                resolved
                    .effective_base_url
                    .as_deref()
                    .unwrap_or(crate::inference::provider::adapter::copilot::ENDPOINT),
                secret,
                &self.counter,
            )?
        } else {
            build_provider(resolved, &effective, &self.counter)?
        };
        *cache = Some(CachedClient {
            fingerprint,
            client: client.clone(),
        });
        Ok(ResolvedProvider {
            client,
            identity: fingerprint,
            method,
            protocols: resolved
                .auth_methods
                .iter()
                .filter(|entry| entry.method == method)
                .flat_map(|entry| entry.protocols.iter().copied())
                .collect(),
        })
    }
}

#[async_trait::async_trait]
impl ModelProvider for PreparedProvider {
    async fn ensure_usable(&self, model: &ModelConfig) -> Result<(), InferenceError> {
        self.resolve_selected(model, true).await.map(|_| ())
    }

    async fn list_models(
        &self,
    ) -> Result<
        crate::inference::provider::ProviderModelList,
        crate::inference::provider::ModelListError,
    > {
        let connection = self
            .connection
            .as_ref()
            .map_err(|error| crate::inference::provider::ModelListError(error.clone()))?;
        let model = ModelConfig {
            catalog_provider: String::new(),
            provider_handle: connection.handle.clone(),
            model_id: "model-list".into(),
            provider: ProviderModel::Generic,
            request_settings: Default::default(),
        };
        self.resolve_selected(&model, false)
            .await
            .map_err(|error| crate::inference::provider::ModelListError(error.to_string()))?
            .client
            .list_models()
            .await
    }

    async fn inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<rig_core::completion::Message>,
        tools: Vec<rig_core::completion::request::ToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<crate::inference::provider::InferenceOutput, InferenceError> {
        self.resolve_selected(model, true)
            .await?
            .client
            .inference(
                model,
                system_prompt,
                chat_history,
                tools,
                max_tokens,
                temperature,
            )
            .await
    }

    async fn stream_inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<rig_core::completion::Message>,
        tools: Vec<rig_core::completion::request::ToolDefinition>,
        token_tx: tokio::sync::mpsc::Sender<crate::inference::provider::StreamToken>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<crate::inference::provider::InferenceOutput, InferenceError> {
        self.resolve_selected(model, true)
            .await?
            .client
            .stream_inference(
                model,
                system_prompt,
                chat_history,
                tools,
                token_tx,
                max_tokens,
                temperature,
            )
            .await
    }

    async fn structured_inference(
        &self,
        model: &ModelConfig,
        system_prompt: &str,
        chat_history: Vec<rig_core::completion::Message>,
        schema: serde_json::Value,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        self.resolve_selected(model, true)
            .await?
            .client
            .structured_inference(
                model,
                system_prompt,
                chat_history,
                schema,
                max_tokens,
                temperature,
            )
            .await
    }
}

fn unavailable(model: &ModelConfig, reason: &str) -> InferenceError {
    InferenceError::InferenceFailed(format!("model '{}' unavailable: {reason}", model.as_str()))
}

fn environment_reference(value: &str) -> Option<&str> {
    value.strip_prefix("${")?.strip_suffix('}')
}

fn model_protocol(model: &ModelConfig) -> ApiSurface {
    match &model.provider {
        ProviderModel::OpenAI { api, .. } => (*api).unwrap_or_default().into(),
        ProviderModel::Anthropic { .. } => ApiSurface::AnthropicMessages,
        ProviderModel::Gemini { .. } => ApiSurface::GoogleGenerateContent,
        ProviderModel::Bedrock { .. } => ApiSurface::AmazonBedrockConverse,
        ProviderModel::Ollama { .. } => ApiSurface::Ollama,
        ProviderModel::Custom { name } if name == "cohere" => ApiSurface::CohereChat,
        ProviderModel::Custom { name } if name == "huggingface" => ApiSurface::HuggingFace,
        _ => ApiSurface::Completions,
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn explicit_credential_replacement_is_live_and_deleted_ids_never_fall_back() {
        use super::*;
        use serde_json::json;
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store =
            crate::credential::managed::test_support::credentials(db.clone(), "secret").await;
        let first = store
            .vault()
            .create("static", json!({}), json!({"API_KEY":"first"}))
            .await
            .unwrap();
        let handle = Handle::const_validated("account");
        let config = ModelProviderConfig {
            provider: Some("openai".into()),
            credential_id: Some(first.item_id),
            ..Default::default()
        };
        let runtime = RuntimeCredentials::new(
            [(handle.clone(), config)].into(),
            store.clone(),
            InferenceCounter::new(crate::chat::broadcast::BroadcastService::new()),
        );
        let old = runtime.resolve_for_listing(&handle).await.unwrap();
        db.query("UPDATE type::record('vault_connection', 'managed') SET enabled = false")
            .await
            .unwrap()
            .check()
            .unwrap();
        assert!(runtime.resolve_for_listing(&handle).await.is_err());
        db.query("UPDATE type::record('vault_connection', 'managed') SET enabled = true")
            .await
            .unwrap()
            .check()
            .unwrap();
        let updated = store
            .vault()
            .replace_by_id(
                first.item_id,
                first.version,
                "static",
                json!({}),
                json!({"API_KEY":"second"}),
            )
            .await
            .unwrap();
        let new = runtime.resolve_for_listing(&handle).await.unwrap();
        assert_ne!(old.identity, new.identity);
        assert!(!Arc::ptr_eq(&old.client, &new.client));
        store
            .vault()
            .delete_by_id(first.item_id, updated.version)
            .await
            .unwrap();
        assert!(runtime.resolve_for_listing(&handle).await.is_err());
        let recreated = store
            .vault()
            .create("static", json!({}), json!({"API_KEY":"third"}))
            .await
            .unwrap();
        assert_ne!(recreated.item_id, first.item_id);
        assert!(runtime.resolve_for_listing(&handle).await.is_err());
    }

    #[tokio::test]
    async fn explicit_credential_lookup_rejects_personal_scope_and_incompatible_integration() {
        use super::*;
        use serde_json::json;
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store =
            crate::credential::managed::test_support::credentials(db.clone(), "secret").await;
        let personal =
            crate::credential::managed::test_support::vault(&db, "alice-vault", "secret").await;
        let private = personal
            .create("static", json!({}), json!({"API_KEY":"private"}))
            .await
            .unwrap();
        let unknown = store
            .vault()
            .create("unknown", json!({}), json!({}))
            .await
            .unwrap();
        for id in [private.item_id, unknown.item_id, uuid::Uuid::new_v4()] {
            let handle = Handle::const_validated("account");
            let runtime = RuntimeCredentials::new(
                [(
                    handle.clone(),
                    ModelProviderConfig {
                        provider: Some("openai".into()),
                        credential_id: Some(id),
                        ..Default::default()
                    },
                )]
                .into(),
                store.clone(),
                InferenceCounter::new(crate::chat::broadcast::BroadcastService::new()),
            );
            assert!(runtime.resolve_for_listing(&handle).await.is_err());
        }
    }

    #[test]
    fn explicit_credential_config_round_trips_and_rejects_conflicting_sources() {
        use super::*;
        let id = uuid::Uuid::new_v4();
        let config: ModelProviderConfig = serde_json::from_value(serde_json::json!({
            "provider":"openai", "credential_id":id
        }))
        .unwrap();
        assert_eq!(config.credential_id, Some(id));
        assert_eq!(
            serde_json::to_value(&config).unwrap()["credential_id"],
            id.to_string()
        );
        assert!(
            serde_json::from_value::<ModelProviderConfig>(serde_json::json!({
                "credential_id":"not-a-uuid"
            }))
            .is_err()
        );
        let mut conflict = config;
        conflict.api_key = Some("${KEY}".into());
        assert!(ProviderPlatform::resolve(&Handle::const_validated("openai"), &conflict).is_err());
    }
    use super::*;
    use crate::chat::broadcast::BroadcastService;
    use crate::core::config::AdapterId;
    use crate::credential::managed::Candidate;
    use crate::inference::credential::store::CredentialBinding;

    struct ResponsesOnly;

    #[async_trait::async_trait]
    impl CredentialMethodDriver for ResponsesOnly {
        fn method(&self) -> CredentialMethod {
            CredentialMethod::Oauth
        }

        fn accepts(
            &self,
            connection: &crate::inference::provider::platform::ResolvedConnection,
        ) -> bool {
            connection.brand == "openai"
                && connection.effective_base_url.as_deref() == Some("https://api.openai.com/v1")
        }

        fn protocols(&self) -> &[ApiSurface] {
            &[ApiSurface::Responses]
        }

        fn build(
            &self,
            connection: &crate::inference::provider::platform::ResolvedConnection,
            config: &ModelProviderConfig,
            secret: &crate::credential::managed::integration::ErasedSecret,
            counter: &InferenceCounter,
        ) -> Result<Arc<dyn ModelProvider>, InferenceError> {
            let rig_core::providers::chatgpt::ChatGPTAuth::AccessToken { access_token, .. } =
                secret
                    .credentials::<rig_core::providers::chatgpt::ChatGPTAuth>()
                    .map_err(|e| InferenceError::ConfigError(e.to_string()))?
            else {
                return Err(InferenceError::ConfigError("expected access token".into()));
            };
            let mut config = config.clone();
            config.api_key = Some(access_token.to_owned());
            build_provider(connection, &config, counter)
        }
    }

    #[tokio::test]
    async fn integration_replacement_preserves_models_and_rejects_incompatible_protocols() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store = crate::credential::managed::test_support::credentials(db, "test-secret").await;
        let handle = Handle::const_validated("openai");
        let mut config = ModelProviderConfig::default();
        let resolved = ProviderPlatform::resolve(&handle, &config).unwrap();
        let binding =
            crate::inference::provider::validation::binding(&resolved, "database", &config);
        let key = store
            .stage(
                &handle,
                CredentialMethod::ApiKey,
                binding.clone(),
                &Candidate::Secret(
                    crate::credential::managed::integration::static_secret::document("key".into()),
                ),
            )
            .await
            .unwrap();
        let active_key = store.promote(key.validation_id, Some(0)).await.unwrap();
        config.credential_id = Some(active_key.item_id);
        let oauth = store
            .stage(
                &handle,
                CredentialMethod::Oauth,
                crate::inference::provider::validation::binding(&resolved, "database", &config),
                &Candidate::Secret(
                    serde_json::to_value(
                        crate::credential::managed::integration::openai_codex::Document {
                            access_token: "token".into(),
                            refresh_token: Some("renewable".into()),
                            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                            account_id: Some("account".into()),
                            scopes: vec![],
                        },
                    )
                    .unwrap(),
                ),
            )
            .await
            .unwrap();

        let resolver = RuntimeCredentials::new(
            HashMap::from([(handle.clone(), config)]),
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        )
        .with_method_driver(Arc::new(ResponsesOnly));
        let completions = ModelConfig {
            catalog_provider: String::new(),
            provider_handle: crate::core::Handle::const_validated("openai"),
            model_id: "test".into(),
            provider: crate::core::config::ProviderModel::from_name("openai"),
            request_settings: Default::default(),
        };
        assert!(
            resolver
                .provider(&completions.provider_handle)
                .unwrap()
                .resolve_selected(&completions, true)
                .await
                .is_ok()
        );
        store.promote(oauth.validation_id, Some(1)).await.unwrap();
        let error = resolver
            .provider(&completions.provider_handle)
            .unwrap()
            .resolve_selected(&completions, true)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("unsupported_protocol_for_auth_method"),
            "{error}"
        );
        let mut responses = completions.clone();
        responses.provider = ProviderModel::OpenAI {
            api: Some(crate::core::config::OpenAiApi::Responses),
            params: Default::default(),
        };
        assert!(
            resolver
                .provider(&responses.provider_handle)
                .unwrap()
                .resolve_selected(&responses, true)
                .await
                .is_ok()
        );
        let listing = resolver.resolve_for_listing(&handle).await.unwrap();
        assert_eq!(listing.method, CredentialMethod::Oauth);
        assert_eq!(listing.protocols, [ApiSurface::Responses]);
        assert_eq!(model_protocol(&completions), ApiSurface::Completions);
    }

    #[tokio::test]
    async fn explicit_credentials_use_each_handles_endpoint() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store = crate::credential::managed::test_support::credentials(db, "test-secret").await;
        let mut configs = HashMap::new();
        let mut models = Vec::new();
        let mut servers = Vec::new();
        for suffix in ["a", "b"] {
            let server = MockServer::start().await;
            let handle = Handle::try_new(format!("account-{suffix}")).unwrap();
            let key = format!("db-{suffix}");
            Mock::given(method("POST")).and(path("/chat/completions"))
                .and(header("authorization", format!("Bearer {key}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id":"response", "object":"chat.completion", "created":0, "model":"test",
                    "choices":[{"index":0,"message":{"role":"assistant","content":suffix},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                }))).expect(1).mount(&server).await;
            let credential = store
                .vault()
                .create(
                    "static",
                    serde_json::json!({}),
                    crate::credential::managed::integration::static_secret::document(key),
                )
                .await
                .unwrap();
            let config = ModelProviderConfig {
                provider: Some("openai".into()),
                credential_id: Some(credential.item_id),
                base_url: Some(server.uri()),
                ..Default::default()
            };
            configs.insert(handle.clone(), config);
            models.push(ModelConfig {
                request_settings: Default::default(),
                catalog_provider: String::new(),
                provider_handle: handle,
                model_id: "test".into(),
                provider: ProviderModel::from_name("openai"),
            });
            servers.push(server);
        }
        let resolver = RuntimeCredentials::new(
            configs,
            store,
            InferenceCounter::new(BroadcastService::new()),
        );
        for model in models {
            let provider = resolver.provider(&model.provider_handle).unwrap();
            let output = provider
                .inference(&model, "test", vec![], vec![], Some(10), None)
                .await
                .unwrap();
            assert_eq!(output.content.len(), 1);
        }
        for server in servers {
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn unresolved_environment_does_not_fall_back_to_database() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store = crate::credential::managed::test_support::credentials(db, "test-secret").await;
        let handle = Handle::const_validated("openai-refresh");
        let config = ModelProviderConfig {
            provider: Some("openai".into()),
            ..Default::default()
        };
        let resolved = ProviderPlatform::resolve(&handle, &config).unwrap();
        let binding =
            crate::inference::provider::validation::binding(&resolved, "database", &config);
        let proof = store
            .stage(
                &handle,
                CredentialMethod::ApiKey,
                binding,
                &Candidate::Secret(
                    crate::credential::managed::integration::static_secret::document(
                        "original".into(),
                    ),
                ),
            )
            .await
            .unwrap();
        store.promote(proof.validation_id, Some(0)).await.unwrap();
        let config = ModelProviderConfig {
            api_key: Some("${MISSING_KEY}".into()),
            ..config
        };
        let mut resolver = RuntimeCredentials::new(
            HashMap::from([(handle.clone(), config)]),
            store,
            InferenceCounter::new(BroadcastService::new()),
        );
        for entry in resolver.entries.values_mut() {
            Arc::get_mut(entry).unwrap().environment = Arc::new(|_| None);
        }
        let model = ModelConfig {
            request_settings: Default::default(),
            catalog_provider: String::new(),
            provider_handle: handle,
            model_id: "test".into(),
            provider: ProviderModel::from_name("openai"),
        };
        let error = resolver
            .provider(&model.provider_handle)
            .unwrap()
            .resolve_selected(&model, true)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("unresolved environment"), "{error}");
    }

    #[tokio::test]
    async fn generation_changes_replace_only_the_affected_client_and_pending_is_invisible() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store = crate::credential::managed::test_support::credentials(db, "test-secret").await;
        let a = Handle::const_validated("openai-a");
        let b = Handle::const_validated("openai-b");
        let config = ModelProviderConfig {
            provider: Some("openai".into()),
            ..Default::default()
        };
        let binding = CredentialBinding {
            provider: "openai".into(),
            adapter: AdapterId::Openai,
            effective_endpoint: Some("https://api.openai.com/v1".into()),
            source: "database".into(),
            authentication_attributes: Default::default(),
        };
        let mut configs = HashMap::new();
        for handle in [&a, &b] {
            let pending = store
                .stage(
                    handle,
                    CredentialMethod::ApiKey,
                    binding.clone(),
                    &Candidate::Secret(
                        crate::credential::managed::integration::static_secret::document(
                            handle.to_string(),
                        ),
                    ),
                )
                .await
                .unwrap();
            let saved = store.promote(pending.validation_id, Some(0)).await.unwrap();
            let mut config = config.clone();
            config.credential_id = Some(saved.item_id);
            configs.insert(handle.clone(), config);
        }
        let binding = crate::inference::provider::validation::binding(
            &ProviderPlatform::resolve(&a, &configs[&a]).unwrap(),
            "database",
            &configs[&a],
        );
        let resolver = RuntimeCredentials::new(
            configs,
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        );
        let model_a = ModelConfig {
            request_settings: Default::default(),
            catalog_provider: String::new(),
            provider_handle: a.clone(),
            model_id: "test".into(),
            provider: ProviderModel::from_name("openai"),
        };
        let model_b = ModelConfig {
            request_settings: Default::default(),
            catalog_provider: String::new(),
            provider_handle: b,
            ..model_a.clone()
        };
        let prepared = resolver.provider(&a).unwrap();
        let client_a = resolver
            .provider(&model_a.provider_handle)
            .unwrap()
            .resolve_selected(&model_a, true)
            .await
            .unwrap()
            .client;
        let client_b = resolver
            .provider(&model_b.provider_handle)
            .unwrap()
            .resolve_selected(&model_b, true)
            .await
            .unwrap()
            .client;
        let draft = store
            .stage(
                &a,
                CredentialMethod::ApiKey,
                binding,
                &Candidate::Secret(
                    crate::credential::managed::integration::static_secret::document(
                        "replacement".into(),
                    ),
                ),
            )
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            &client_a,
            &resolver
                .provider(&model_a.provider_handle)
                .unwrap()
                .resolve_selected(&model_a, true)
                .await
                .unwrap()
                .client
        ));
        store.promote(draft.validation_id, Some(1)).await.unwrap();
        assert!(Arc::ptr_eq(&prepared, &resolver.provider(&a).unwrap()));
        prepared.ensure_usable(&model_a).await.unwrap();
        assert!(!Arc::ptr_eq(
            &client_a,
            &resolver
                .provider(&model_a.provider_handle)
                .unwrap()
                .resolve_selected(&model_a, true)
                .await
                .unwrap()
                .client
        ));
        assert!(Arc::ptr_eq(
            &client_b,
            &resolver
                .provider(&model_b.provider_handle)
                .unwrap()
                .resolve_selected(&model_b, true)
                .await
                .unwrap()
                .client
        ));
        let current = store
            .vault()
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.item_id == draft.item_id)
            .unwrap();
        store
            .vault()
            .delete_by_id(current.item_id, current.version)
            .await
            .unwrap();
        assert!(
            resolver
                .provider(&model_a.provider_handle)
                .unwrap()
                .resolve_selected(&model_a, true)
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(&prepared, &resolver.provider(&a).unwrap()));
        assert!(prepared.ensure_usable(&model_a).await.is_err());
    }

    #[tokio::test]
    async fn shared_references_observe_replacement_and_deletion_without_affecting_inline_auth() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store = crate::credential::managed::test_support::credentials(db, "secret").await;
        let first = store
            .vault()
            .create(
                "static",
                serde_json::json!({}),
                crate::credential::managed::integration::static_secret::document("first".into()),
            )
            .await
            .unwrap();
        let a = Handle::const_validated("account-a");
        let b = Handle::const_validated("account-b");
        let other = Handle::const_validated("inline-account");
        let config = ModelProviderConfig {
            provider: Some("openai".into()),
            credential_id: Some(first.item_id),
            ..Default::default()
        };
        let runtime = RuntimeCredentials::new(
            [
                (a.clone(), config.clone()),
                (b.clone(), config),
                (
                    other.clone(),
                    ModelProviderConfig {
                        provider: Some("openai".into()),
                        api_key: Some("inline".into()),
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        );
        let before_a = runtime.resolve_for_listing(&a).await.unwrap();
        let before_b = runtime.resolve_for_listing(&b).await.unwrap();
        let before_other = runtime.resolve_for_listing(&other).await.unwrap();
        let updated = store
            .vault()
            .replace_by_id(
                first.item_id,
                first.version,
                "static",
                serde_json::json!({}),
                crate::credential::managed::integration::static_secret::document("updated".into()),
            )
            .await
            .unwrap();
        assert_ne!(
            before_a.identity,
            runtime.resolve_for_listing(&a).await.unwrap().identity
        );
        assert_ne!(
            before_b.identity,
            runtime.resolve_for_listing(&b).await.unwrap().identity
        );
        assert_eq!(
            before_other.identity,
            runtime.resolve_for_listing(&other).await.unwrap().identity
        );
        store
            .vault()
            .delete_by_id(updated.item_id, updated.version)
            .await
            .unwrap();
        assert!(runtime.resolve_for_listing(&a).await.is_err());
        assert!(runtime.resolve_for_listing(&b).await.is_err());
        assert!(runtime.resolve_for_listing(&other).await.is_ok());
    }

    #[tokio::test]
    async fn provider_lookup_does_not_implicitly_select_a_newly_saved_credential() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store = crate::credential::managed::test_support::credentials(db, "test-secret").await;
        let handle = Handle::const_validated("openai");
        let config = ModelProviderConfig::default();
        let binding = crate::inference::provider::validation::binding(
            &ProviderPlatform::resolve(&handle, &config).unwrap(),
            "database",
            &config,
        );
        let runtime = RuntimeCredentials::new(
            [(handle.clone(), config)].into(),
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        );
        let provider = runtime.provider(&handle).unwrap();
        assert!(provider.cache.lock().await.is_none());
        let model = ModelConfig {
            catalog_provider: String::new(),
            provider_handle: crate::core::Handle::const_validated("openai"),
            model_id: "test".into(),
            provider: crate::core::config::ProviderModel::from_name("openai"),
            request_settings: Default::default(),
        };
        assert!(provider.ensure_usable(&model).await.is_err());
        let pending = store
            .stage(
                &handle,
                CredentialMethod::ApiKey,
                binding,
                &Candidate::Secret(
                    crate::credential::managed::integration::static_secret::document(
                        "fixture-key".into(),
                    ),
                ),
            )
            .await
            .unwrap();
        store.promote(pending.validation_id, Some(0)).await.unwrap();
        assert!(provider.ensure_usable(&model).await.is_err());
        assert!(Arc::ptr_eq(&provider, &runtime.provider(&handle).unwrap()));
    }
}
