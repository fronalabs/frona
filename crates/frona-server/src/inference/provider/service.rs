#[cfg(test)]
use crate::credential::managed::Candidate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};
use uuid::Uuid;

use crate::core::{
    Handle,
    config::{
        AdapterId, ApiSurface, Config, ConfigService, ModelGroupConfig, ModelProviderConfig,
        SaveResult, redact_config_for_api, validate_document,
    },
    error::AppError,
};
use crate::inference::credential::store::{
    CredentialMethod, CredentialState, CredentialStatus, PendingCredentialStatus,
    ProviderCredentials,
};
use crate::inference::{
    credential::runtime::EffectiveAuthentication,
    provider::{
        platform::ProviderPlatform,
        registry::ModelProviderRegistry,
        validation::{
            CandidateCredential, ProviderValidationError, ProviderValidationService,
            ValidationCandidate,
        },
    },
};

#[derive(Clone)]
pub struct ModelProviderService {
    pub directory: crate::inference::directory::models::ModelDirectoryService,
    pub store: ProviderCredentials,
    pub validator: ProviderValidationService,
    pub config_service: ConfigService,
    registry: ModelProviderRegistry,
    pub runtime: Arc<crate::inference::credential::runtime::RuntimeCredentials>,
    pub active: Arc<Config>,
}

#[derive(Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialInput {
    ApiKey { api_key: String },
    Database { method: CredentialMethod },
    Environment { variable: String },
    Ambient { method: CredentialMethod },
    Anonymous,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateRequest {
    pub config: ModelProviderConfig,
    pub credential: CredentialInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftRequest {
    #[serde(default)]
    pub manual_models: Vec<String>,
    pub config: ModelProviderConfig,
    pub validation_id: Uuid,
    pub method: CredentialMethod,
    pub source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialModelsRequest {
    pub config: ModelProviderConfig,
    #[serde(default)]
    pub manual_models: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub config: ModelProviderConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogoutRequest {
    pub expected_generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    pub config: ModelProviderConfig,
    pub expected_persisted_revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequest {
    pub expected_persisted_revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub config: ModelProviderConfig,
    pub method: CredentialMethod,
}

#[derive(Serialize)]
pub struct PublicCredential {
    pub credential_id: Option<Uuid>,
    pub method: CredentialMethod,
    pub state: CredentialState,
    pub generation: u64,
    pub version: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_id: Option<Uuid>,
}

impl From<CredentialStatus> for PublicCredential {
    fn from(status: CredentialStatus) -> Self {
        Self {
            credential_id: Some(status.item_id),
            method: status.method,
            state: status.state,
            generation: status.generation,
            version: status.version,
            validation_id: None,
        }
    }
}

impl From<PendingCredentialStatus> for PublicCredential {
    fn from(status: PendingCredentialStatus) -> Self {
        Self {
            credential_id: None,
            method: status.method,
            state: status.state,
            generation: status.base_generation,
            version: status.version,
            validation_id: Some(status.validation_id),
        }
    }
}

#[derive(Serialize)]
pub struct SavedCredential {
    pub credential_id: Uuid,
    pub integration: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct AuthenticationMethodInfo {
    pub id: String,
    pub method: CredentialMethod,
    pub priority: u16,
    pub protocols: Vec<ApiSurface>,
    pub login_available: bool,
    pub validation_available: bool,
}

#[derive(Serialize)]
pub struct ProviderInspection {
    pub setup: Option<crate::inference::directory::providers::ProviderCatalogEntry>,
    pub handle: Handle,
    pub provider: String,
    pub adapter: AdapterId,
    pub configuration: Value,
    pub pending_removal: bool,
    pub effective_authentication: Option<EffectiveAuthentication>,
    pub authentication_methods: Vec<AuthenticationMethodInfo>,
    pub credentials: Vec<PublicCredential>,
    pub affected_groups: Vec<String>,
}

#[derive(Serialize)]
pub struct ValidationResult {
    pub validation_id: Uuid,
    pub credential: PublicCredential,
    pub models: Option<Vec<crate::inference::directory::models::ProviderModelInfo>>,
}

pub use crate::inference::directory::models::ModelListing;

#[derive(Serialize)]
pub struct MutationResult {
    pub credential: PublicCredential,
    pub affected_groups: Vec<String>,
    pub unavailable_models: HashMap<String, Vec<(String, String)>>,
}

pub fn referenced_groups(config: &Config, handle: &Handle) -> Vec<String> {
    fn references(model: &ModelGroupConfig, handle: &Handle) -> bool {
        model.provider == *handle
            || model
                .common
                .fallbacks
                .iter()
                .any(|fallback| references(fallback, handle))
    }
    let mut groups: Vec<String> = config
        .models
        .iter()
        .filter(|(_, model)| references(model, handle))
        .map(|(name, _)| name.clone())
        .collect();
    groups.sort();
    groups
}

impl ModelProviderService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        directory: crate::inference::directory::models::ModelDirectoryService,
        config_service: ConfigService,
        store: ProviderCredentials,
        validator: ProviderValidationService,
        runtime: Arc<crate::inference::credential::runtime::RuntimeCredentials>,
        active: Arc<Config>,
        groups: HashMap<String, crate::inference::ModelGroup>,
        providers: HashMap<String, Arc<dyn crate::inference::provider::ModelProvider>>,
    ) -> Self {
        let registry = ModelProviderRegistry::new(providers, groups);
        Self {
            directory,
            config_service,
            store,
            validator,
            runtime,
            active,
            registry,
        }
    }

    pub fn resolve(
        &self,
        reference: &crate::inference::ModelRef,
    ) -> Result<crate::inference::ModelGroup, crate::inference::InferenceError> {
        self.registry.resolve(reference)
    }

    pub fn resolve_with_fallback(
        &self,
        reference: &crate::inference::ModelRef,
        fallback: &crate::inference::ModelRef,
    ) -> Result<crate::inference::ModelGroup, crate::inference::InferenceError> {
        self.registry.resolve_with_fallback(reference, fallback)
    }

    pub fn compile(
        &self,
        mut model: crate::inference::ModelConfig,
        retry: crate::inference::config::RetryConfig,
        context_budget: usize,
    ) -> Result<crate::inference::ModelGroup, crate::inference::InferenceError> {
        if context_budget == 0 || model.model_id.is_empty() {
            return Err(crate::inference::InferenceError::ConfigError(
                "model ID and positive context budget are required".into(),
            ));
        }
        self.runtime.prepare_model(&mut model)?;
        Ok(crate::inference::ModelGroup {
            providers: self.registry.providers().clone(),
            name: model.as_str(),
            max_tokens: model.request_settings.max_tokens,
            temperature: model.request_settings.temperature,
            main: model,
            fallbacks: Vec::new(),
            context_window: context_budget,
            retry,
            inference: self.active.inference.clone(),
        })
    }

    fn persisted(&self) -> Result<Config, AppError> {
        validate_document(&self.config_service.persisted()?.config)
    }

    pub async fn inspect(
        &self,
        handle: &Handle,
        draft: Option<ModelProviderConfig>,
    ) -> Result<ProviderInspection, AppError> {
        let persisted = self.persisted()?;
        let saved = persisted.providers.get(handle);
        let active = self.active.providers.get(handle);
        let config = draft
            .or_else(|| saved.cloned())
            .or_else(|| active.cloned())
            .ok_or_else(|| AppError::NotFound("provider not found".into()))?;
        let resolved = ProviderPlatform::resolve(handle, &config).map_err(invalid)?;
        let effective = if active.is_some() {
            Some(
                self.runtime
                    .effective_authentication(handle)
                    .await
                    .unwrap_or(EffectiveAuthentication {
                        method: None,
                        source: "unavailable".into(),
                    }),
            )
        } else {
            None
        };
        let mut statuses = Vec::new();
        if let Some(id) = config.credential_id
            && let Some(status) = self.store.vault().status_by_id(id).await?
            && let Ok(method) = credential_method(&status.integration)
        {
            statuses.push(PublicCredential {
                credential_id: Some(id),
                method,
                state: CredentialState::Active,
                generation: status.generation,
                version: status.version,
                validation_id: None,
            });
        }
        let mut redacted = json!({"providers":{handle.as_str():config}});
        redact_config_for_api(&mut redacted);
        let groups: BTreeSet<String> = referenced_groups(&persisted, handle)
            .into_iter()
            .chain(referenced_groups(&self.active, handle))
            .collect();
        Ok(ProviderInspection {
            setup: None,
            handle: handle.clone(),
            provider: resolved.brand.clone(),
            adapter: resolved.adapter,
            configuration: redacted["providers"][handle.as_str()].take(),
            pending_removal: saved.is_none() && active.is_some(),
            effective_authentication: effective,
            authentication_methods: resolved
                .auth_methods
                .iter()
                .map(|method| AuthenticationMethodInfo {
                    id: method.id.into(),
                    method: method.method,
                    priority: method.priority,
                    protocols: method.protocols.to_vec(),
                    login_available: crate::inference::credential::setup::provider(
                        &resolved,
                        method.method,
                    )
                    .is_some(),
                    validation_available: ProviderPlatform::has_validator(&resolved, method.method),
                })
                .collect(),
            credentials: statuses,
            affected_groups: groups.into_iter().collect(),
        })
    }

    pub async fn list(&self) -> Result<Vec<ProviderInspection>, AppError> {
        let persisted = self.persisted()?;
        let mut handles: Vec<Handle> = persisted
            .providers
            .keys()
            .cloned()
            .chain(self.active.providers.keys().cloned())
            .collect();
        handles.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        handles.dedup();
        let mut result = Vec::new();
        for handle in handles {
            let item = self.inspect(&handle, None).await?;
            result.push(item);
        }
        Ok(result)
    }

    pub async fn validate(
        &self,
        user_id: &str,
        handle: &Handle,
        request: ValidateRequest,
    ) -> Result<ValidationResult, AppError> {
        if user_id.is_empty() {
            return Err(AppError::Forbidden(
                "draft validation requires a user".into(),
            ));
        }
        let connection = ProviderPlatform::resolve(handle, &request.config).map_err(invalid)?;
        let credential = match request.credential {
            CredentialInput::ApiKey { api_key } => CandidateCredential::New {
                method: crate::inference::credential::store::CredentialMethod::ApiKey,
                document: if connection.brand == "github-copilot" {
                    serde_json::to_value(
                        crate::credential::managed::integration::copilot::Document {
                            github_token: api_key,
                        },
                    )
                    .map_err(|_| AppError::Validation("invalid Copilot credential".into()))?
                } else {
                    crate::credential::managed::integration::static_secret::document(api_key)
                },
            },
            CredentialInput::Database { method } => {
                CandidateCredential::ExistingDatabase { method }
            }
            CredentialInput::Environment { variable } => {
                CandidateCredential::Environment { variable }
            }
            CredentialInput::Ambient { method } => CandidateCredential::Ambient { method },
            CredentialInput::Anonymous => CandidateCredential::Anonymous,
        };
        let proof = self
            .validator
            .validate(ValidationCandidate {
                handle: handle.clone(),
                config: request.config,
                credential,
            })
            .await
            .map_err(validation_error)?;
        let method = proof.pending.method;
        self.store
            .claim_draft(proof.pending.validation_id, user_id)
            .await?;
        let eligible: Vec<_> = connection
            .auth_methods
            .iter()
            .filter(|entry| entry.method == method)
            .flat_map(|entry| entry.protocols.iter().copied())
            .collect();
        let configured = self.configured_models(handle)?;
        Ok(ValidationResult {
            validation_id: proof.pending.validation_id,
            credential: proof.pending.into(),
            models: proof
                .models
                .map(|models| {
                    self.finish_listing(self.directory.listing(
                        &connection,
                        Some(method),
                        &eligible,
                        configured,
                        &[],
                        crate::inference::directory::models::inventory(models),
                    ))
                    .map(|listing| listing.models)
                })
                .transpose()?,
        })
    }

    /// Accept a validated credential before saving any configuration reference.
    pub async fn accept(
        &self,
        user_id: &str,
        handle: &Handle,
        request: DraftRequest,
    ) -> Result<PublicCredential, AppError> {
        self.store
            .authorize_draft(request.validation_id, user_id)
            .await?;
        let connection = ProviderPlatform::resolve(handle, &request.config).map_err(invalid)?;
        let expected = super::validation::binding(&connection, &request.source, &request.config);
        self.validator
            .resolve_proof(request.validation_id, handle, request.method, &expected)
            .await
            .map_err(validation_error)?;
        let saved = self.store.promote(request.validation_id, None).await?;
        Ok(saved.into())
    }

    pub async fn saved_credentials(&self) -> Result<Vec<SavedCredential>, AppError> {
        Ok(self
            .store
            .vault()
            .list()
            .await?
            .into_iter()
            .filter(|status| !status.removed)
            .map(|status| SavedCredential {
                credential_id: status.item_id,
                name: status
                    .metadata
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&status.key.connection)
                    .to_owned(),
                integration: status.integration,
            })
            .collect())
    }

    pub async fn draft_models(
        &self,
        user_id: &str,
        handle: &Handle,
        draft: DraftRequest,
    ) -> Result<ModelListing, AppError> {
        self.store
            .authorize_draft(draft.validation_id, user_id)
            .await?;
        validate_manual_models(&draft.manual_models)?;
        let connection = ProviderPlatform::resolve(handle, &draft.config).map_err(invalid)?;
        let provider = self
            .validator
            .draft_provider(
                draft.validation_id,
                handle,
                &draft.config,
                draft.method,
                &draft.source,
            )
            .await
            .map_err(validation_error)?;
        let inventory = self.directory.draft_inventory(provider.as_ref()).await;
        let eligible: Vec<_> = connection
            .auth_methods
            .iter()
            .filter(|method| method.method == draft.method)
            .flat_map(|method| method.protocols.iter().copied())
            .collect();
        self.finish_listing(self.directory.listing(
            &connection,
            Some(draft.method),
            &eligible,
            self.configured_models(handle)?,
            &draft.manual_models,
            inventory,
        ))
    }

    pub async fn credential_models(
        &self,
        handle: &Handle,
        request: CredentialModelsRequest,
    ) -> Result<ModelListing, AppError> {
        if request.config.credential_id.is_none() {
            return Err(AppError::Validation(
                "a saved credential is required".into(),
            ));
        }
        validate_manual_models(&request.manual_models)?;
        let connection = ProviderPlatform::resolve(handle, &request.config).map_err(invalid)?;
        let resolved = self
            .runtime
            .resolve_config_for_listing(handle, request.config)
            .await
            .map_err(invalid)?;
        let inventory = self
            .directory
            .draft_inventory(resolved.client.as_ref())
            .await;
        self.finish_listing(self.directory.listing(
            &connection,
            Some(resolved.method),
            &resolved.protocols,
            self.configured_models(handle)?,
            &request.manual_models,
            inventory,
        ))
    }

    pub async fn active_models(&self, handle: &Handle) -> Result<ModelListing, AppError> {
        self.active_models_with_manual(handle, &[]).await
    }

    pub async fn active_models_with_manual(
        &self,
        handle: &Handle,
        manual: &[String],
    ) -> Result<ModelListing, AppError> {
        validate_manual_models(manual)?;
        let persisted = self.persisted()?;
        let config = self
            .active
            .providers
            .get(handle)
            .or_else(|| persisted.providers.get(handle))
            .ok_or_else(|| AppError::NotFound("provider not found".into()))?;
        let connection = ProviderPlatform::resolve(handle, config).map_err(invalid)?;
        let runtime = &self.runtime;
        let (inventory, method, eligible) = match runtime.resolve_for_listing(handle).await {
            Ok(resolved) => (
                self.directory.active_inventory(handle, &resolved).await,
                Some(resolved.method),
                resolved.protocols,
            ),
            Err(_) => (
                crate::inference::directory::models::Inventory::Error,
                None,
                vec![],
            ),
        };
        self.finish_listing(self.directory.listing(
            &connection,
            method,
            &eligible,
            self.configured_models(handle)?,
            manual,
            inventory,
        ))
    }

    fn configured_models(
        &self,
        handle: &Handle,
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>, AppError> {
        let mut result =
            crate::inference::directory::models::configured_models(handle, &self.active.models);
        for (id, groups) in crate::inference::directory::models::configured_models(
            handle,
            &self.persisted()?.models,
        ) {
            let entry = result.entry(id).or_default();
            entry.extend(groups);
            entry.sort();
            entry.dedup();
        }
        Ok(result)
    }

    fn finish_listing(&self, mut listing: ModelListing) -> Result<ModelListing, AppError> {
        crate::inference::directory::models::retain_saved_protocols(
            &mut listing,
            &self.active.models,
        );
        crate::inference::directory::models::retain_saved_protocols(
            &mut listing,
            &self.persisted()?.models,
        );
        Ok(listing)
    }

    pub async fn logout(
        &self,
        handle: &Handle,
        method: CredentialMethod,
        request: LogoutRequest,
    ) -> Result<MutationResult, AppError> {
        let persisted = self.persisted()?;
        let config = persisted
            .providers
            .get(handle)
            .or_else(|| self.active.providers.get(handle))
            .ok_or_else(|| AppError::NotFound("provider not found".into()))?;
        let id = config
            .credential_id
            .ok_or_else(|| AppError::Validation("provider has no managed credential ID".into()))?;
        let current = self
            .store
            .vault()
            .status_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound("managed credential not found".into()))?;
        if current.generation != request.expected_generation
            || credential_method(&current.integration)? != method
        {
            return Err(AppError::Conflict("credential changed".into()));
        }
        self.store.vault().delete_by_id(id, current.version).await?;
        let connection = ProviderPlatform::resolve(handle, config).map_err(invalid)?;
        let status = CredentialStatus {
            handle: handle.clone(),
            method,
            state: CredentialState::Removed,
            generation: current.generation,
            version: current.version,
            item_id: id,
            binding: super::validation::binding(&connection, "database", config),
            updated_at: chrono::Utc::now(),
        };
        self.mutation_result(handle, status).await
    }

    async fn mutation_result(
        &self,
        handle: &Handle,
        status: CredentialStatus,
    ) -> Result<MutationResult, AppError> {
        Ok(MutationResult {
            credential: status.into(),
            affected_groups: referenced_groups(&self.active, handle),
            unavailable_models: self.registry.unavailable_models().await,
        })
    }

    pub async fn edit(
        &self,
        handle: &Handle,
        request: EditRequest,
    ) -> Result<SaveResult, AppError> {
        if request
            .config
            .api_key
            .as_deref()
            .is_some_and(|key| !(key.starts_with("${") && key.ends_with('}')))
        {
            return Err(AppError::Validation(
                "submit API keys to credential validation, not provider configuration".into(),
            ));
        }
        // The dedicated route accepts a complete entry. Explicit nulls remove
        // omitted optional attributes rather than retaining the old endpoint.
        let mut entry = serde_json::to_value(&request.config).map_err(invalid)?;
        if let Some(old) = self
            .config_service
            .persisted()?
            .config
            .get("providers")
            .and_then(|providers| providers.get(handle.as_str()))
            .and_then(Value::as_object)
        {
            for key in old.keys() {
                let omitted = Value::Null;
                entry
                    .as_object_mut()
                    .expect("provider object")
                    .entry(key.clone())
                    .or_insert(omitted);
            }
        }
        self.config_service
            .save(
                json!({"providers":{handle.as_str():entry}}),
                Some(&request.expected_persisted_revision),
            )
            .await
    }

    pub async fn delete(
        &self,
        handle: &Handle,
        request: DeleteRequest,
    ) -> Result<SaveResult, AppError> {
        let persisted = self.persisted()?;
        if !persisted.providers.contains_key(handle) {
            return Err(AppError::NotFound("provider not found".into()));
        }
        let references = referenced_groups(&persisted, handle);
        if !references.is_empty() {
            return Err(AppError::Conflict(format!(
                "provider is referenced by model groups: {}",
                references.join(", ")
            )));
        }
        self.config_service
            .save(
                json!({"providers":{handle.as_str():null}}),
                Some(&request.expected_persisted_revision),
            )
            .await
    }

    pub async fn discard(
        &self,
        user_id: &str,
        handle: &Handle,
        validation_id: Uuid,
    ) -> Result<bool, AppError> {
        self.store.authorize_draft(validation_id, user_id).await?;
        let (pending, _) = self
            .store
            .pending(validation_id)
            .await?
            .ok_or_else(|| AppError::NotFound("validation proof not found".into()))?;
        if pending.handle != *handle {
            return Err(AppError::Validation(
                "validation proof belongs to a different handle".into(),
            ));
        }
        self.store.discard_pending(validation_id).await?;
        Ok(self.store.pending(validation_id).await?.is_none())
    }

    pub async fn prepare_login(
        &self,
        handle: &Handle,
        request: LoginRequest,
    ) -> Result<
        (
            &'static str,
            crate::credential::managed::login::service::LoginTarget,
        ),
        AppError,
    > {
        let connection = ProviderPlatform::resolve(handle, &request.config).map_err(invalid)?;
        if !crate::inference::credential::setup::provider(&connection, request.method).is_some() {
            return Err(unsupported_login());
        }
        let metadata = if let Some(id) = request.config.credential_id {
            self.store
                .vault()
                .status_by_id(id)
                .await?
                .ok_or_else(|| AppError::NotFound("managed credential".into()))?
                .metadata
        } else {
            json!({"name":handle})
        };
        let target = crate::credential::managed::login::service::LoginTarget {
            vault: self.store.vault().clone(),
            key: crate::credential::managed::Key::new(handle.to_string(), request.method.as_str()),
            expected_id: request.config.credential_id,
            metadata,
        };
        Ok((
            crate::inference::credential::setup::provider(&connection, request.method)
                .ok_or_else(unsupported_login)?,
            target,
        ))
    }
}

fn credential_method(integration: &str) -> Result<CredentialMethod, AppError> {
    match integration {
        "static" => Ok(CredentialMethod::ApiKey),
        "copilot" | "openai_codex" => Ok(CredentialMethod::Oauth),
        _ => Err(AppError::Validation(
            "unsupported credential integration".into(),
        )),
    }
}

pub fn validate_manual_models(models: &[String]) -> Result<(), AppError> {
    if models.len() > 100
        || models.iter().any(|id| {
            id.is_empty() || id.trim() != id || id.len() > 1024 || id.chars().any(char::is_control)
        })
    {
        return Err(AppError::Validation("manual_models: expected at most 100 nonempty model IDs without surrounding whitespace or control characters".into()));
    }
    Ok(())
}

fn invalid(error: impl std::fmt::Display) -> AppError {
    AppError::Validation(error.to_string())
}

fn unsupported_login() -> AppError {
    AppError::Http {
        status: 501,
        message: "login is not implemented for this authentication method".into(),
    }
}

fn validation_error(error: ProviderValidationError) -> AppError {
    match error {
        ProviderValidationError::Unsupported => AppError::Http {
            status: 501,
            message: "live validation is not implemented for this authentication method".into(),
        },
        ProviderValidationError::Storage(_) => {
            AppError::Database("provider credential storage failed".into())
        }
        other => AppError::Validation(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::broadcast::BroadcastService,
        inference::{
            config::ModelRegistryConfig,
            provider::{InferenceCounter, validation::binding},
        },
    };

    fn handle() -> Handle {
        Handle::const_validated("account")
    }

    async fn fixture() -> (ModelProviderService, tempfile::TempDir) {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        let mut config = Config::default();
        config.providers.insert(
            handle(),
            ModelProviderConfig {
                provider: Some("openai".into()),
                base_url: Some("http://127.0.0.1:9".into()),
                ..Default::default()
            },
        );
        let store =
            crate::credential::managed::test_support::credentials(db.clone(), "synthetic-secret")
                .await;
        let credential = store
            .vault()
            .create(
                "static",
                json!({"name":"Account"}),
                crate::credential::managed::integration::static_secret::document(
                    "synthetic-provider-key".into(),
                ),
            )
            .await
            .unwrap();
        config.providers.get_mut(&handle()).unwrap().credential_id = Some(credential.item_id);
        std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let validator = ProviderValidationService::new(
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        );
        let mut loaded = ConfigService::load(&path).unwrap();
        loaded.config.auth.encryption_secret = "synthetic-secret".into();
        let config_service = ConfigService::new(loaded).unwrap();
        let runtime = Arc::new(
            crate::inference::credential::runtime::RuntimeCredentials::new(
                config.providers.clone(),
                store.clone(),
                InferenceCounter::new(BroadcastService::new()),
            ),
        );
        let groups = ModelRegistryConfig {
            providers: config.providers.clone(),
            models: config.models.clone(),
            skip_auto_discover: true,
        }
        .parse_model_groups_with_catalog(
            &config.inference,
            &frona_model_catalog::ModelCatalogSnapshot::empty(),
            Arc::new(runtime.providers()),
        )
        .unwrap();
        let providers = runtime.providers();
        let service = ModelProviderService::new(
            crate::inference::directory::models::ModelDirectoryService::new(
                frona_model_catalog::sources::CatalogSources::load(directory.path()),
            ),
            config_service,
            store,
            validator,
            runtime,
            Arc::new(config),
            groups,
            providers,
        );
        (service, directory)
    }

    #[tokio::test]
    async fn saved_subscription_lists_models_before_connection_is_saved() {
        let (mut service, _directory) = fixture().await;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("authorization", "Bearer fixture-access-token"))
            .and(wiremock::matchers::header("chatgpt-account-id", "fixture-account"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"models":[{"slug":"fresh-model","display_name":"Fresh model","visibility":"list"}]})))
            .expect(1).mount(&server).await;
        service.runtime = Arc::new(
            crate::inference::credential::runtime::RuntimeCredentials::new(
                HashMap::new(),
                service.store.clone(),
                InferenceCounter::new(BroadcastService::new()),
            )
            .with_method_driver(Arc::new(
                crate::inference::provider::adapter::chatgpt::Driver::for_test(server.uri()),
            )),
        );
        let credential = service
            .store
            .vault()
            .create(
                "openai_codex",
                json!({"name":"New subscription"}),
                json!({"access_token":"fixture-access-token", "refresh_token":"fixture-refresh-token",
                "expires_at":chrono::Utc::now() + chrono::Duration::hours(1),
                "account_id":"fixture-account", "scopes":[]}),
            )
            .await
            .unwrap();
        let handle = Handle::const_validated("new-subscription");
        let config = ModelProviderConfig {
            provider: Some("openai".into()),
            credential_id: Some(credential.item_id),
            ..Default::default()
        };
        let listing = service
            .credential_models(
                &handle,
                CredentialModelsRequest {
                    config: config.clone(),
                    manual_models: vec![],
                },
            )
            .await
            .unwrap();
        assert!(!listing.models.is_empty());
        assert_eq!(listing.models[0].id, "fresh-model");
        assert_eq!(listing.source, "account");
        let payload = serde_json::to_value(listing).unwrap();
        assert_eq!(payload["credential_method"], "oauth");
        assert!(payload["models"].as_array().unwrap().iter().any(|model| {
            model["protocols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|protocol| protocol["api"] == "responses" && protocol["available"] == true)
        }));
        assert!(!service.active.providers.contains_key(&handle));
        assert!(!service.persisted().unwrap().providers.contains_key(&handle));
        assert!(!payload.to_string().contains("fixture-access-token"));
        for config in [
            ModelProviderConfig {
                credential_id: None,
                ..config.clone()
            },
            ModelProviderConfig {
                credential_id: Some(Uuid::new_v4()),
                ..config.clone()
            },
            ModelProviderConfig {
                provider: Some("anthropic".into()),
                ..config.clone()
            },
            ModelProviderConfig {
                base_url: Some("https://custom.invalid/v1".into()),
                ..config
            },
        ] {
            assert!(
                service
                    .credential_models(
                        &handle,
                        CredentialModelsRequest {
                            config,
                            manual_models: vec![]
                        }
                    )
                    .await
                    .is_err()
            );
        }
    }

    async fn stage(service: &ModelProviderService, payload: Candidate) -> Uuid {
        let config = service.active.providers.get(&handle()).unwrap();
        let resolved = ProviderPlatform::resolve(&handle(), config).unwrap();
        let method = match payload {
            Candidate::Secret(ref doc) if doc.get("access_token").is_some() => {
                CredentialMethod::Oauth
            }
            _ => CredentialMethod::ApiKey,
        };
        let id = service
            .store
            .stage(
                &handle(),
                method,
                binding(&resolved, "database", config),
                &payload,
            )
            .await
            .unwrap()
            .validation_id;
        service.store.claim_draft(id, "admin").await.unwrap();
        id
    }

    #[tokio::test]
    async fn draft_operations_enforce_owner_without_route_coordination() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let (service, _directory) = fixture().await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(|request: &wiremock::Request| {
                if request
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    == Some("Bearer fixture-key")
                {
                    ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[]}))
                } else {
                    ResponseTemplate::new(401)
                }
            })
            .mount(&server)
            .await;
        let config = ModelProviderConfig {
            provider: Some("openai".into()),
            base_url: Some(server.uri()),
            ..Default::default()
        };
        let proof = service
            .validate(
                "owner",
                &handle(),
                ValidateRequest {
                    config: config.clone(),
                    credential: CredentialInput::ApiKey {
                        api_key: "fixture-key".into(),
                    },
                },
            )
            .await
            .unwrap();
        let request = || DraftRequest {
            manual_models: vec![],
            config: config.clone(),
            validation_id: proof.validation_id,
            method: CredentialMethod::ApiKey,
            source: "database".into(),
        };
        let calls = server.received_requests().await.unwrap().len();
        assert!(
            service
                .draft_models("other", &handle(), request())
                .await
                .is_err()
        );
        assert!(service.accept("other", &handle(), request()).await.is_err());
        assert!(
            service
                .discard("other", &handle(), proof.validation_id)
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), calls);
        assert!(
            service
                .store
                .pending(proof.validation_id)
                .await
                .unwrap()
                .is_some()
        );
        service
            .draft_models("owner", &handle(), request())
            .await
            .unwrap();
        assert!(
            service
                .discard("owner", &handle(), proof.validation_id)
                .await
                .unwrap()
        );
        assert!(
            service
                .draft_models("owner", &handle(), request())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn accepted_credentials_survive_failed_saves_and_provider_removal() {
        let (service, directory) = fixture().await;
        let id = stage(
            &service,
            Candidate::Secret(
                crate::credential::managed::integration::static_secret::document(
                    "accepted-secret".into(),
                ),
            ),
        )
        .await;
        let config = service.active.providers[&handle()].clone();
        let request = || DraftRequest {
            manual_models: vec![],
            config: config.clone(),
            validation_id: id,
            method: CredentialMethod::ApiKey,
            source: "database".into(),
        };
        assert!(
            service
                .accept("different-admin", &handle(), request())
                .await
                .is_err()
        );
        let accepted = service.accept("admin", &handle(), request()).await.unwrap();
        let credential_id = accepted.credential_id.unwrap();
        assert!(service.accept("admin", &handle(), request()).await.is_err());
        let mut config = config;
        config.credential_id = Some(credential_id);
        assert!(
            service
                .edit(
                    &handle(),
                    EditRequest {
                        config: config.clone(),
                        expected_persisted_revision: "stale-file".into()
                    }
                )
                .await
                .is_err()
        );
        assert!(
            service
                .saved_credentials()
                .await
                .unwrap()
                .iter()
                .any(|entry| entry.credential_id == credential_id)
        );
        let saved = service
            .edit(
                &handle(),
                EditRequest {
                    config,
                    expected_persisted_revision: service
                        .config_service
                        .persisted()
                        .unwrap()
                        .persisted_revision,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            saved.config["providers"]["account"]["credential_id"],
            credential_id.to_string()
        );
        service
            .delete(
                &handle(),
                DeleteRequest {
                    expected_persisted_revision: saved.persisted_revision,
                },
            )
            .await
            .unwrap();
        assert!(
            service
                .store
                .vault()
                .status_by_id(credential_id)
                .await
                .unwrap()
                .is_some()
        );
        std::fs::write(directory.path().join("config.yaml"), "{}\n").unwrap();
        assert!(
            service
                .saved_credentials()
                .await
                .unwrap()
                .iter()
                .any(|entry| entry.credential_id == credential_id)
        );
    }

    #[tokio::test]
    async fn deletion_checks_primary_and_fallback_references() {
        let (service, _directory) = fixture().await;
        service.config_service.save(json!({"models":{"primary":{"provider":"account","model":"test","fallbacks":[{"provider":"account","model":"fallback"}]}}}), None).await.unwrap();
        let result = service
            .delete(
                &handle(),
                DeleteRequest {
                    expected_persisted_revision: service
                        .config_service
                        .persisted()
                        .unwrap()
                        .persisted_revision,
                },
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("primary"));
        assert!(
            service
                .config_service
                .save(json!({"providers":{"account":null}}), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn inspection_does_not_publish_drafts_or_secret_material() {
        let (service, _directory) = fixture().await;
        let id = stage(
            &service,
            Candidate::Secret(
                crate::credential::managed::integration::static_secret::document(
                    "draft-secret".into(),
                ),
            ),
        )
        .await;
        let inspection = service.inspect(&handle(), None).await.unwrap();
        let encoded = serde_json::to_string(&inspection).unwrap();
        assert!(!encoded.contains("synthetic-provider-key"));
        assert!(!encoded.contains("draft-secret"));
        assert!(!encoded.contains(&id.to_string()));
        assert!(
            service
                .discard("admin", &Handle::const_validated("different"), id)
                .await
                .is_err()
        );
        assert!(service.discard("admin", &handle(), id).await.unwrap());
    }

    #[tokio::test]
    async fn named_lookup_is_exact_and_ad_hoc_compilation_requires_explicit_controls() {
        use crate::core::config::{OpenAiApi, ProviderModel};
        use crate::inference::{
            ModelConfig, ModelRef, config::RetryConfig, provider::ModelRequestSettings,
        };
        let (mut service, _directory) = fixture().await;
        let model: ModelGroupConfig = serde_json::from_value(json!({
            "provider":"account", "model":"fixture", "api":"responses"
        }))
        .unwrap();
        service.registry = {
            let registry_config = ModelRegistryConfig {
                providers: service.active.providers.clone(),
                models: [("primary".into(), model.clone()), ("title".into(), model)].into(),
                skip_auto_discover: true,
            };
            let runtime = service.runtime.clone();
            let groups = registry_config
                .parse_model_groups_with_catalog(
                    &service.active.inference,
                    &frona_model_catalog::ModelCatalogSnapshot::empty(),
                    Arc::new(runtime.providers()),
                )
                .unwrap();
            crate::inference::provider::registry::ModelProviderRegistry::new(
                runtime.providers(),
                groups,
            )
        };
        service
            .logout(
                &handle(),
                CredentialMethod::ApiKey,
                LogoutRequest {
                    expected_generation: 1,
                },
            )
            .await
            .unwrap();
        let primary = ModelRef::PRIMARY;
        assert!(service.resolve(&ModelRef("Primary".into())).is_err());
        assert!(service.resolve(&ModelRef("missing".into())).is_err());
        assert!(
            service
                .resolve_with_fallback(&ModelRef(String::new().into()), &primary)
                .is_err()
        );
        assert!(
            service
                .resolve_with_fallback(
                    &ModelRef("missing".into()),
                    &ModelRef("also-missing".into())
                )
                .is_err()
        );
        assert_eq!(
            service
                .resolve_with_fallback(&ModelRef("missing".into()), &primary)
                .unwrap()
                .name,
            "primary"
        );
        assert_eq!(
            service
                .resolve_with_fallback(&ModelRef::TITLE, &primary)
                .unwrap()
                .name,
            "title"
        );
        let config = ModelConfig {
            catalog_provider: String::new(),
            provider_handle: handle(),
            model_id: "ad-hoc".into(),
            provider: ProviderModel::OpenAI {
                api: Some(OpenAiApi::Responses),
                params: Default::default(),
            },
            request_settings: ModelRequestSettings::default(),
        };
        let group = service
            .compile(
                config.clone(),
                RetryConfig {
                    max_retries: 7,
                    ..Default::default()
                },
                2048,
            )
            .unwrap();
        assert_eq!(group.context_window, 2048);
        assert_eq!(group.retry.max_retries, 7);
        assert_eq!(group.main.catalog_provider, "openai");
        assert_eq!(
            service.resolve(&primary).unwrap().main.catalog_provider,
            "openai"
        );
        assert!(group.fallbacks.is_empty());
        assert!(service.compile(config, RetryConfig::default(), 0).is_err());
    }
}
