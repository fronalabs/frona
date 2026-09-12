use crate::credential::managed::Candidate;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::core::Handle;
use crate::core::config::ModelProviderConfig;
use crate::inference::credential::store::{
    CredentialBinding, CredentialMethod, PendingCredentialStatus, ProviderCredentials,
};

use crate::inference::provider::platform::{ProviderPlatform, build_provider};
use crate::inference::provider::{
    CredentialValidationError, InferenceCounter, ModelProvider, ProviderModelList,
};

#[derive(Clone)]
pub enum CandidateCredential {
    New {
        method: CredentialMethod,
        document: serde_json::Value,
    },
    ExistingDatabase {
        method: CredentialMethod,
    },
    Environment {
        variable: String,
    },
    Ambient {
        method: CredentialMethod,
    },
    Anonymous,
}

impl std::fmt::Debug for CandidateCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::New { method, .. } => f
                .debug_struct("New")
                .field("method", method)
                .field("document", &"[REDACTED]")
                .finish(),
            Self::ExistingDatabase { method } => {
                f.debug_tuple("ExistingDatabase").field(method).finish()
            }
            Self::Environment { variable } => f.debug_tuple("Environment").field(variable).finish(),
            Self::Ambient { method } => f.debug_tuple("Ambient").field(method).finish(),
            Self::Anonymous => f.write_str("Anonymous"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ValidationCandidate {
    pub handle: Handle,
    pub config: ModelProviderConfig,
    pub credential: CandidateCredential,
}

#[derive(Debug, Clone)]
pub struct ValidationProof {
    pub pending: PendingCredentialStatus,
    pub models: Option<ProviderModelList>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderValidationError {
    #[error("credential validation is unsupported for this adapter")]
    Unsupported,
    #[error("credential validation timed out after {0}ms")]
    TimedOut(u64),
    #[error("credential validation was rejected: {0}")]
    Rejected(String),
    #[error("invalid provider candidate: {0}")]
    InvalidCandidate(String),
    #[error("validation proof '{0}' was not found or has expired")]
    ProofNotFound(Uuid),
    #[error("validation proof does not match the provider draft: {0}")]
    ProofMismatch(String),
    #[error("validation proof was superseded by a credential change")]
    ProofSuperseded,
    #[error("credential storage failed: {0}")]
    Storage(String),
}

#[derive(Clone)]
pub struct ProviderValidationService {
    store: ProviderCredentials,
    counter: InferenceCounter,
    timeout: Duration,
    method_drivers: Vec<Arc<dyn crate::inference::credential::runtime::CredentialMethodDriver>>,
}

impl ProviderValidationService {
    pub fn new(store: ProviderCredentials, counter: InferenceCounter) -> Self {
        Self {
            store,
            counter,
            timeout: Duration::from_secs(15),
            method_drivers: vec![
                Arc::new(crate::inference::provider::adapter::copilot::Driver::default()),
                Arc::new(crate::inference::provider::adapter::chatgpt::Driver::default()),
            ],
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_method_driver(
        mut self,
        driver: Arc<dyn crate::inference::credential::runtime::CredentialMethodDriver>,
    ) -> Self {
        self.method_drivers.insert(0, driver);
        self
    }

    async fn session_provider(
        &self,
        connection: &crate::inference::provider::platform::ResolvedConnection,
        config: &ModelProviderConfig,
        method: CredentialMethod,
        document: &serde_json::Value,
    ) -> Result<Arc<dyn ModelProvider>, ProviderValidationError> {
        let secret = if connection.brand == "github-copilot" {
            let doc: crate::credential::managed::integration::copilot::Document =
                serde_json::from_value(document.clone())
                    .map_err(|_| ProviderValidationError::Unsupported)?;
            crate::credential::managed::integration::copilot::CopilotIntegration::default()
                .exchange(&doc.github_token)
                .await
                .map_err(|_| ProviderValidationError::Unsupported)?
                .erase()
        } else {
            let doc: crate::credential::managed::integration::openai_codex::Document =
                serde_json::from_value(document.clone())
                    .map_err(|_| ProviderValidationError::Unsupported)?;
            doc.resolved()
                .map_err(|_| ProviderValidationError::Unsupported)?
                .erase()
        };
        self.method_drivers
            .iter()
            .find(|driver| {
                let session_method = if connection.brand == "github-copilot" {
                    CredentialMethod::Oauth
                } else {
                    method
                };
                driver.method() == session_method && driver.accepts(connection)
            })
            .ok_or(ProviderValidationError::Unsupported)?
            .build(connection, config, &secret, &self.counter)
            .map_err(|_| {
                ProviderValidationError::InvalidCandidate(
                    "cannot construct this session for the selected connection".into(),
                )
            })
    }

    pub async fn validate(
        &self,
        candidate: ValidationCandidate,
    ) -> Result<ValidationProof, ProviderValidationError> {
        let prepared = self.prepare(candidate).await?;
        // The document has already supplied the request key. Resolve the adapter
        // without treating that key as a second configured credential source.
        let mut effective = prepared.config.clone();
        effective.credential_id = None;
        let resolved = ProviderPlatform::resolve(&prepared.handle, &effective)
            .map_err(|error| ProviderValidationError::InvalidCandidate(error.to_string()))?;
        if !resolved
            .auth_methods
            .iter()
            .any(|descriptor| descriptor.method == prepared.method)
        {
            return Err(ProviderValidationError::Unsupported);
        }
        let binding = binding(&resolved, &prepared.source, &prepared.config);
        let provider = build_provider(&resolved, &prepared.config, &self.counter)
            .map_err(|error| ProviderValidationError::InvalidCandidate(error.to_string()))?;
        // Public lists cannot establish authentication merely because the
        // request contained a key. Require an authentication rejection for an
        // unrelated control key before accepting listing as credential proof.
        if prepared.method == CredentialMethod::ApiKey {
            let mut control_config = prepared.config.clone();
            control_config.api_key = Some(format!("frona-validation-control-{}", Uuid::new_v4()));
            let control = build_provider(&resolved, &control_config, &self.counter)
                .map_err(|error| ProviderValidationError::InvalidCandidate(error.to_string()))?;
            match tokio::time::timeout(self.timeout, control.validate_credentials()).await {
                Ok(Err(CredentialValidationError::AuthenticationRejected)) => {}
                Ok(Err(CredentialValidationError::Unsupported)) => return Err(ProviderValidationError::Unsupported),
                Err(_) => return Err(ProviderValidationError::TimedOut(self.timeout.as_millis() as u64)),
                _ => return Err(ProviderValidationError::Rejected(
                    "model listing did not establish authentication; this endpoint needs a dedicated credential check".into()
                )),
            }
        }
        self.validate_prepared(prepared, binding, provider).await
    }

    async fn prepare(
        &self,
        mut candidate: ValidationCandidate,
    ) -> Result<PreparedCandidate, ProviderValidationError> {
        let connection = ProviderPlatform::resolve(&candidate.handle, &candidate.config)
            .map_err(|error| ProviderValidationError::InvalidCandidate(error.to_string()))?;
        let (method, source, proof_payload) = match candidate.credential {
            CandidateCredential::New { method, document } => {
                apply_document(&mut candidate.config, &connection.brand, method, &document)?;
                (method, "database".to_string(), Candidate::Secret(document))
            }
            CandidateCredential::ExistingDatabase { method } => {
                let (status, payload) = self
                    .store
                    .active_id(candidate.config.credential_id)
                    .await
                    .map_err(storage)?
                    .ok_or_else(|| {
                        ProviderValidationError::InvalidCandidate(format!(
                            "no active {} credential exists for '{}'",
                            method.as_str(),
                            candidate.handle
                        ))
                    })?;
                apply_document(&mut candidate.config, &connection.brand, method, &payload)?;
                (
                    method,
                    "database".to_string(),
                    Candidate::Existing(status.version),
                )
            }
            CandidateCredential::Environment { variable } => {
                let value = std::env::var(&variable).map_err(|_| {
                    ProviderValidationError::InvalidCandidate(format!(
                        "environment variable '{variable}' is not set"
                    ))
                })?;
                candidate.config.api_key = Some(value);
                (
                    CredentialMethod::ApiKey,
                    format!("environment:{variable}"),
                    Candidate::External,
                )
            }
            CandidateCredential::Ambient { method } => {
                candidate.config.api_key = None;
                (method, "ambient".to_string(), Candidate::External)
            }
            CandidateCredential::Anonymous => (
                CredentialMethod::Anonymous,
                "anonymous".to_string(),
                Candidate::External,
            ),
        };
        Ok(PreparedCandidate {
            handle: candidate.handle,
            config: candidate.config,
            method,
            source,
            proof_payload,
        })
    }

    async fn validate_prepared(
        &self,
        prepared: PreparedCandidate,
        binding: CredentialBinding,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<ValidationProof, ProviderValidationError> {
        let base_generation = self
            .store
            .active_id(prepared.config.credential_id)
            .await
            .map_err(storage)?
            .map_or(0, |(status, _)| status.generation);
        let result = tokio::time::timeout(self.timeout, provider.validate_credentials())
            .await
            .map_err(|_| ProviderValidationError::TimedOut(self.timeout.as_millis() as u64))?
            .map_err(|error| match error {
                CredentialValidationError::AuthenticationRejected => {
                    ProviderValidationError::Rejected("provider rejected authentication".into())
                }
                CredentialValidationError::Unsupported => ProviderValidationError::Unsupported,
                CredentialValidationError::Failed(message) => {
                    ProviderValidationError::Rejected(message)
                }
            })?;

        let models = result.models;
        let pending = self
            .store
            .stage(
                &prepared.handle,
                prepared.method,
                binding,
                &prepared.proof_payload,
            )
            .await
            .map_err(storage)?;
        if pending.base_generation != base_generation {
            self.store
                .discard_pending(pending.validation_id)
                .await
                .map_err(storage)?;
            return Err(ProviderValidationError::ProofSuperseded);
        }
        Ok(ValidationProof { pending, models })
    }

    pub async fn resolve_proof(
        &self,
        validation_id: Uuid,
        expected_handle: &Handle,
        expected_method: CredentialMethod,
        expected_binding: &CredentialBinding,
    ) -> Result<(PendingCredentialStatus, Candidate), ProviderValidationError> {
        let (status, payload) = self
            .store
            .pending(validation_id)
            .await
            .map_err(storage)?
            .ok_or(ProviderValidationError::ProofNotFound(validation_id))?;
        if chrono::Utc::now() - status.created_at > chrono::Duration::minutes(30) {
            return Err(ProviderValidationError::ProofNotFound(validation_id));
        }
        if &status.handle != expected_handle {
            return Err(ProviderValidationError::ProofMismatch(format!(
                "handle is '{}', expected '{}'",
                status.handle, expected_handle
            )));
        }
        if status.method != expected_method {
            return Err(ProviderValidationError::ProofMismatch(format!(
                "method is '{}', expected '{}'",
                status.method.as_str(),
                expected_method.as_str()
            )));
        }
        if &status.binding != expected_binding {
            return Err(ProviderValidationError::ProofMismatch(
                "provider, adapter, endpoint, source, or authentication attributes changed"
                    .to_string(),
            ));
        }
        let credential_id = expected_binding
            .authentication_attributes
            .get("credential_id")
            .and_then(|id| id.as_str())
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| ProviderValidationError::ProofSuperseded)?;
        let current = self.store.active_id(credential_id).await.map_err(storage)?;
        if current.as_ref().map_or(0, |(active, _)| active.generation) != status.base_generation {
            return Err(ProviderValidationError::ProofSuperseded);
        }
        if let Candidate::Existing(expected_version) = payload {
            if !current.is_some_and(|(active, _)| active.version == expected_version) {
                return Err(ProviderValidationError::ProofSuperseded);
            }
            return Ok((status, Candidate::Existing(expected_version)));
        }
        Ok((status, payload))
    }

    pub async fn draft_provider(
        &self,
        validation_id: Uuid,
        handle: &Handle,
        config: &ModelProviderConfig,
        method: CredentialMethod,
        source: &str,
    ) -> Result<Arc<dyn ModelProvider>, ProviderValidationError> {
        let resolved = ProviderPlatform::resolve(handle, config)
            .map_err(|error| ProviderValidationError::InvalidCandidate(error.to_string()))?;
        let expected = binding(&resolved, source, config);
        let (_, payload) = self
            .resolve_proof(validation_id, handle, method, &expected)
            .await?;
        let mut effective = config.clone();
        match payload {
            Candidate::Existing(version) => {
                let (active, payload) = self
                    .store
                    .active_id(config.credential_id)
                    .await
                    .map_err(storage)?
                    .ok_or(ProviderValidationError::ProofSuperseded)?;
                if active.version != version {
                    return Err(ProviderValidationError::ProofSuperseded);
                }
                let document = &payload;
                if method == CredentialMethod::Oauth || resolved.brand == "github-copilot" {
                    return self
                        .session_provider(&resolved, config, method, document)
                        .await;
                }
                apply_document(&mut effective, &resolved.brand, method, document)?;
            }
            Candidate::External => {
                if let Some(variable) = source.strip_prefix("environment:") {
                    effective.api_key = Some(std::env::var(variable).map_err(|_| {
                        ProviderValidationError::InvalidCandidate(format!(
                            "environment variable '{variable}' is not set"
                        ))
                    })?);
                } else if source == "ambient" && method == CredentialMethod::Aws {
                    // Match ambient validation: use the AWS chain even if the
                    // draft still contains a key from its previous method.
                    effective.api_key = None;
                } else if method != CredentialMethod::Anonymous {
                    return Err(ProviderValidationError::Unsupported);
                }
            }
            Candidate::Secret(document) => {
                if method == CredentialMethod::Oauth || resolved.brand == "github-copilot" {
                    return self
                        .session_provider(&resolved, config, method, &document)
                        .await;
                }
                apply_document(&mut effective, &resolved.brand, method, &document)?;
            }
        }
        build_provider(&resolved, &effective, &self.counter)
            .map_err(|error| ProviderValidationError::InvalidCandidate(error.to_string()))
    }
}

struct PreparedCandidate {
    handle: Handle,
    config: ModelProviderConfig,
    method: CredentialMethod,
    source: String,
    proof_payload: Candidate,
}

fn apply_document(
    config: &mut ModelProviderConfig,
    brand: &str,
    method: CredentialMethod,
    document: &serde_json::Value,
) -> Result<(), ProviderValidationError> {
    if method != CredentialMethod::ApiKey {
        return Err(ProviderValidationError::InvalidCandidate(
            "this method requires integration login".into(),
        ));
    }
    if brand == "github-copilot" {
        let doc: crate::credential::managed::integration::copilot::Document =
            serde_json::from_value(document.clone())
                .map_err(|_| ProviderValidationError::Unsupported)?;
        if doc.github_token.is_empty() {
            return Err(ProviderValidationError::Unsupported);
        }
        config.api_key = Some(doc.github_token);
        return Ok(());
    }
    let fields: crate::credential::managed::integration::static_secret::Document =
        serde_json::from_value(document.clone())
            .map_err(|_| ProviderValidationError::Unsupported)?;
    config.api_key = Some(
        crate::credential::managed::integration::static_secret::api_key(&fields)
            .map_err(|_| ProviderValidationError::Unsupported)?
            .to_owned(),
    );
    Ok(())
}

pub fn binding(
    connection: &crate::inference::provider::platform::ResolvedConnection,
    source: &str,
    config: &ModelProviderConfig,
) -> CredentialBinding {
    let mut authentication_attributes = config.attributes.clone();
    if let Some(id) = config.credential_id {
        authentication_attributes.insert("credential_id".into(), id.to_string().into());
    }
    for (name, value) in [
        ("aws_profile", &config.aws_profile),
        ("aws_region", &config.aws_region),
        ("azure_credential", &config.azure_credential),
    ] {
        if let Some(value) = value {
            authentication_attributes.insert(name.into(), value.clone().into());
        }
    }
    CredentialBinding {
        provider: connection.brand.clone(),
        adapter: connection.adapter,
        effective_endpoint: connection.effective_base_url.clone(),
        source: source.into(),
        authentication_attributes,
    }
}

fn storage(error: impl std::fmt::Display) -> ProviderValidationError {
    ProviderValidationError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use rig_core::completion::Message as RigMessage;
    use surrealdb::Surreal;
    use surrealdb::engine::local::{Db, Mem};
    use tokio::sync::mpsc;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::chat::broadcast::BroadcastService;
    use crate::inference::error::InferenceError;
    use crate::inference::provider::{
        CredentialValidation, InferenceOutput, ModelConfig, StreamToken,
    };

    #[derive(Clone)]
    enum Check {
        Success,
        Failure,
        Unsupported,
        Hang,
    }

    struct StubProvider(Check);

    #[async_trait]
    impl ModelProvider for StubProvider {
        async fn validate_credentials(
            &self,
        ) -> Result<CredentialValidation, CredentialValidationError> {
            match self.0 {
                Check::Success => Ok(CredentialValidation { models: None }),
                Check::Failure => Err(CredentialValidationError::Failed("401".into())),
                Check::Unsupported => Err(CredentialValidationError::Unsupported),
                Check::Hang => std::future::pending().await,
            }
        }

        async fn inference(
            &self,
            _: &ModelConfig,
            _: &str,
            _: Vec<RigMessage>,
            _: Vec<rig_core::completion::request::ToolDefinition>,
            _: Option<u64>,
            _: Option<f64>,
        ) -> Result<InferenceOutput, InferenceError> {
            unreachable!()
        }

        async fn stream_inference(
            &self,
            _: &ModelConfig,
            _: &str,
            _: Vec<RigMessage>,
            _: Vec<rig_core::completion::request::ToolDefinition>,
            _: mpsc::Sender<StreamToken>,
            _: Option<u64>,
            _: Option<f64>,
        ) -> Result<InferenceOutput, InferenceError> {
            unreachable!()
        }

        async fn structured_inference(
            &self,
            _: &ModelConfig,
            _: &str,
            _: Vec<RigMessage>,
            _: serde_json::Value,
            _: Option<u64>,
            _: Option<f64>,
        ) -> Result<serde_json::Value, InferenceError> {
            unreachable!()
        }
    }

    async fn service() -> (ProviderValidationService, ProviderCredentials) {
        let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store =
            crate::credential::managed::test_support::credentials(db, "validation-test-secret")
                .await;
        let service = ProviderValidationService::new(
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        )
        .with_timeout(Duration::from_millis(20));
        (service, store)
    }

    fn openai_candidate(endpoint: String, key: &str) -> ValidationCandidate {
        ValidationCandidate {
            handle: Handle::const_validated("openai-stage"),
            config: ModelProviderConfig {
                provider: Some("openai".into()),
                base_url: Some(endpoint),
                ..Default::default()
            },
            credential: CandidateCredential::New {
                method: crate::inference::credential::store::CredentialMethod::ApiKey,
                document: crate::credential::managed::integration::static_secret::document(
                    key.into(),
                ),
            },
        }
    }

    fn prepared(secret: &str) -> PreparedCandidate {
        PreparedCandidate {
            handle: Handle::const_validated("openai-stage"),
            config: ModelProviderConfig::default(),
            method: CredentialMethod::ApiKey,
            source: "database".into(),
            proof_payload: Candidate::Secret(
                crate::credential::managed::integration::static_secret::document(secret.into()),
            ),
        }
    }

    fn test_binding(endpoint: &str) -> CredentialBinding {
        CredentialBinding {
            provider: "openai".into(),
            adapter: crate::core::config::AdapterId::Openai,
            effective_endpoint: Some(endpoint.into()),
            source: "database".into(),
            authentication_attributes: Default::default(),
        }
    }

    #[tokio::test]
    async fn copilot_validation_preserves_its_integration_document() {
        let (service, _) = service().await;
        let document =
            serde_json::to_value(crate::credential::managed::integration::copilot::Document {
                github_token: "private-github-token".into(),
            })
            .unwrap();
        let candidate = ValidationCandidate {
            handle: Handle::const_validated("copilot-account"),
            config: ModelProviderConfig {
                provider: Some("github-copilot".into()),
                ..Default::default()
            },
            credential: CandidateCredential::New {
                method: CredentialMethod::ApiKey,
                document: document.clone(),
            },
        };
        assert!(!format!("{:?}", candidate.credential).contains("private-github-token"));
        let prepared = service.prepare(candidate.clone()).await.unwrap();
        assert_eq!(
            prepared.config.api_key.as_deref(),
            Some("private-github-token")
        );
        assert!(matches!(prepared.proof_payload, Candidate::Secret(doc) if doc == document));

        let invalid = ValidationCandidate {
            credential: CandidateCredential::New {
                method: CredentialMethod::ApiKey,
                document: crate::credential::managed::integration::static_secret::document(
                    "wrong-format".into(),
                ),
            },
            ..candidate
        };
        assert!(service.prepare(invalid).await.is_err());
    }

    #[tokio::test]
    async fn only_success_stages_a_candidate() {
        let (service, store) = service().await;
        let attempts = AtomicUsize::new(0);
        for (check, expected) in [
            (Check::Failure, "rejected"),
            (Check::Unsupported, "unsupported"),
            (Check::Hang, "timed out"),
        ] {
            let result = service
                .validate_prepared(
                    prepared("bad"),
                    test_binding("https://a.example/v1"),
                    Arc::new(StubProvider(check)),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(result.contains(expected), "{result}");
            attempts.fetch_add(1, Ordering::Relaxed);
        }
        let success = service
            .validate_prepared(
                prepared("good"),
                test_binding("https://a.example/v1"),
                Arc::new(StubProvider(Check::Success)),
            )
            .await
            .unwrap();
        assert!(
            store
                .pending(success.pending.validation_id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn proof_is_bound_exactly_and_competing_drafts_are_isolated() {
        let (service, _) = service().await;
        let a_binding = test_binding("https://a.example/v1");
        let b_binding = test_binding("https://b.example/v1");
        let a = service
            .validate_prepared(
                prepared("a"),
                a_binding.clone(),
                Arc::new(StubProvider(Check::Success)),
            )
            .await
            .unwrap();
        let b = service
            .validate_prepared(
                prepared("b"),
                b_binding.clone(),
                Arc::new(StubProvider(Check::Success)),
            )
            .await
            .unwrap();
        assert_ne!(a.pending.validation_id, b.pending.validation_id);
        service
            .resolve_proof(
                a.pending.validation_id,
                &a.pending.handle,
                CredentialMethod::ApiKey,
                &a_binding,
            )
            .await
            .unwrap();
        service
            .resolve_proof(
                b.pending.validation_id,
                &b.pending.handle,
                CredentialMethod::ApiKey,
                &b_binding,
            )
            .await
            .unwrap();
        let error = service
            .resolve_proof(
                a.pending.validation_id,
                &a.pending.handle,
                CredentialMethod::ApiKey,
                &b_binding,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match"), "{error}");

        let mut changed_bindings = Vec::new();
        let mut changed = a_binding.clone();
        changed.provider = "other".into();
        changed_bindings.push(changed);
        let mut changed = a_binding.clone();
        changed.adapter = crate::core::config::AdapterId::Anthropic;
        changed_bindings.push(changed);
        let mut changed = a_binding.clone();
        changed.source = "environment:OPENAI_API_KEY".into();
        changed_bindings.push(changed);
        let mut changed = a_binding.clone();
        changed
            .authentication_attributes
            .insert("account".into(), serde_json::json!("two"));
        changed_bindings.push(changed);
        for changed in changed_bindings {
            assert!(
                service
                    .resolve_proof(
                        a.pending.validation_id,
                        &a.pending.handle,
                        CredentialMethod::ApiKey,
                        &changed
                    )
                    .await
                    .is_err()
            );
        }
        assert!(
            service
                .resolve_proof(
                    a.pending.validation_id,
                    &Handle::const_validated("other-handle"),
                    CredentialMethod::ApiKey,
                    &a_binding
                )
                .await
                .is_err()
        );
        assert!(
            service
                .resolve_proof(
                    a.pending.validation_id,
                    &a.pending.handle,
                    CredentialMethod::Oauth,
                    &a_binding
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn proof_only_payload_has_no_external_secret() {
        let (service, store) = service().await;
        for source in ["environment:KEY", "ambient", "anonymous"] {
            let mut candidate = prepared("not-stored");
            candidate.source = source.into();
            candidate.proof_payload = Candidate::External;
            let proof = service
                .validate_prepared(
                    candidate,
                    test_binding("https://a.example/v1"),
                    Arc::new(StubProvider(Check::Success)),
                )
                .await
                .unwrap();
            let (_, payload) = store
                .pending(proof.pending.validation_id)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(payload, Candidate::External));
        }
    }

    #[tokio::test]
    async fn validating_replacement_does_not_change_active_and_active_proofs_expire() {
        let (service, store) = service().await;
        let handle = Handle::const_validated("openai-stage");
        let mut binding = test_binding("https://a.example/v1");
        let initial = store
            .stage(
                &handle,
                CredentialMethod::ApiKey,
                binding.clone(),
                &Candidate::Secret(
                    crate::credential::managed::integration::static_secret::document(
                        "key-a".into(),
                    ),
                ),
            )
            .await
            .unwrap();
        let active_a = store.promote(initial.validation_id, Some(0)).await.unwrap();
        binding
            .authentication_attributes
            .insert("credential_id".into(), active_a.item_id.to_string().into());
        let mut candidate = prepared("key-b");
        candidate.config.credential_id = Some(active_a.item_id);
        let mut replacement_binding = test_binding("https://b.example/v1");
        replacement_binding
            .authentication_attributes
            .insert("credential_id".into(), active_a.item_id.to_string().into());

        let active_proof = store
            .stage(
                &handle,
                CredentialMethod::ApiKey,
                binding.clone(),
                &Candidate::Existing(active_a.version),
            )
            .await
            .unwrap();
        let replacement = service
            .validate_prepared(
                candidate,
                replacement_binding,
                Arc::new(StubProvider(Check::Success)),
            )
            .await
            .unwrap();
        let (still_active, payload) = store
            .active_id(Some(active_a.item_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still_active.generation, active_a.generation);
        assert_eq!(
            payload,
            crate::credential::managed::integration::static_secret::document("key-a".into())
        );
        assert!(
            store
                .pending(replacement.pending.validation_id)
                .await
                .unwrap()
                .is_some()
        );

        let promoted_b = store
            .promote(replacement.pending.validation_id, Some(active_a.generation))
            .await
            .unwrap();
        assert_ne!(promoted_b.version, active_a.version);
        let error = service
            .resolve_proof(
                active_proof.validation_id,
                &handle,
                CredentialMethod::ApiKey,
                &binding,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderValidationError::ProofSuperseded));
    }

    #[tokio::test]
    async fn openai_live_check_accepts_authenticated_list_and_rejects_bad_responses() {
        let accepted = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&accepted)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer good-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "id": "model-a", "owned_by": "test" }]
            })))
            .mount(&accepted)
            .await;
        let (accepted_service, _) = service().await;
        let proof = accepted_service
            // The successful HTTP exchange is not the timeout assertion below.
            .with_timeout(Duration::from_secs(2))
            .validate(openai_candidate(accepted.uri(), "good-key"))
            .await
            .unwrap();
        let Some(ProviderModelList::Listed {
            source: crate::inference::provider::ProviderModelListSource::Account,
            models,
        }) = proof.models
        else {
            panic!("expected account models")
        };
        assert_eq!(models.data[0].id, "model-a");

        for (public, template) in [
            (
                true,
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [{"id": "public-model"}]
                })),
            ),
            (
                false,
                ResponseTemplate::new(401).set_body_string("invalid key"),
            ),
            (
                false,
                ResponseTemplate::new(200).set_body_string("not-json"),
            ),
            (
                false,
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(serde_json::json!({ "data": [] })),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .respond_with(ResponseTemplate::new(401))
                .with_priority(10)
                .mount(&server)
                .await;
            let mut mock = Mock::given(method("GET")).and(path("/models"));
            if !public {
                mock = mock.and(header("authorization", "Bearer bad-key"));
            }
            mock.respond_with(template).mount(&server).await;
            let (service, store) = service().await;
            assert!(
                service
                    .validate(openai_candidate(server.uri(), "bad-key"))
                    .await
                    .is_err()
            );
            assert!(
                store
                    .pending_status(&Handle::const_validated("openai-stage"))
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
