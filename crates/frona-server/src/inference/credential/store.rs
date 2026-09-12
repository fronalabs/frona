use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use surrealdb::Surreal;
#[cfg(test)]
use surrealdb::engine::local::Db;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::core::Handle;
use crate::core::config::AdapterId;
use crate::core::error::AppError;
use crate::credential::managed::{self, Candidate, ManagedVault};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialBinding {
    pub provider: String,
    pub adapter: AdapterId,
    pub effective_endpoint: Option<String>,
    pub source: String,
    #[serde(default)]
    pub authentication_attributes: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialState {
    Active,
    Pending,
    Removed,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialStatus {
    pub handle: Handle,
    pub method: CredentialMethod,
    pub state: CredentialState,
    pub generation: u64,
    pub version: Uuid,
    pub item_id: Uuid,
    pub binding: CredentialBinding,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingCredentialStatus {
    #[serde(skip)]
    pub(crate) owner: Option<String>,
    pub base_generation: u64,
    pub validation_id: Uuid,
    pub handle: Handle,
    pub method: CredentialMethod,
    pub state: CredentialState,
    pub version: Uuid,
    pub item_id: Uuid,
    pub binding: CredentialBinding,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct ProviderCredentials {
    vault: ManagedVault,
    drafts: Arc<Mutex<HashMap<Uuid, (PendingCredentialStatus, Candidate)>>>,
    pub(crate) resolver: Arc<managed::resolver::ManagedResolver>,
}

impl ProviderCredentials {
    pub fn new(vault: ManagedVault, resolver: Arc<managed::resolver::ManagedResolver>) -> Self {
        assert_eq!(
            vault.connection_id(),
            managed::GLOBAL_CONNECTION_ID,
            "model providers require the global managed vault"
        );
        Self {
            vault,
            drafts: Default::default(),
            resolver,
        }
    }

    pub fn vault(&self) -> &ManagedVault {
        &self.vault
    }

    pub(crate) async fn resolve_credential(
        &self,
        id: Uuid,
    ) -> Result<(managed::Status, managed::integration::ErasedSecret), AppError> {
        // Provider configuration may consume only the server-owned provider vault.
        // The global connection lookup must never search personal vaults by a known UUID.
        self.resolver
            .resolve(&self.vault, id, || async { Ok(()) })
            .await
    }

    /// Configure the immutable integration registry during startup.
    pub fn with_integrations(
        mut self,
        integrations: HashMap<
            String,
            Arc<dyn crate::credential::managed::integration::RegisteredIntegration>,
        >,
    ) -> Self {
        self.resolver = Arc::new(managed::resolver::ManagedResolver::new(integrations));
        self
    }

    pub(crate) async fn active_id(
        &self,
        id: Option<Uuid>,
    ) -> Result<Option<(managed::Status, serde_json::Value)>, AppError> {
        let Some(id) = id else {
            return Ok(None);
        };
        let Some(status) = self.vault.status_by_id(id).await? else {
            return Ok(None);
        };
        if status.removed {
            return Ok(None);
        }
        self.vault.document_by_id(id).await.map(Some)
    }

    pub async fn stage(
        &self,
        handle: &Handle,
        method: CredentialMethod,
        binding: CredentialBinding,
        candidate: &Candidate,
    ) -> Result<PendingCredentialStatus, AppError> {
        let current = match binding
            .authentication_attributes
            .get("credential_id")
            .and_then(|id| id.as_str())
        {
            Some(id) => {
                self.vault
                    .status_by_id(Uuid::parse_str(id).map_err(internal)?)
                    .await?
            }
            None => None,
        };
        let pending = PendingCredentialStatus {
            owner: None,
            base_generation: current.as_ref().map_or(0, |status| status.generation),
            validation_id: Uuid::new_v4(),
            handle: handle.clone(),
            method,
            state: CredentialState::Pending,
            version: current
                .as_ref()
                .map_or_else(Uuid::new_v4, |status| status.version),
            item_id: current
                .as_ref()
                .map_or_else(Uuid::new_v4, |status| status.item_id),
            binding,
            created_at: Utc::now(),
        };
        let mut drafts = self.drafts.lock().await;
        drafts.retain(|_, (status, _)| {
            Utc::now() - status.created_at < chrono::Duration::minutes(30)
        });
        if drafts.len() >= 64 {
            return Err(AppError::Conflict("too many credential drafts".into()));
        }
        drafts.insert(pending.validation_id, (pending.clone(), candidate.clone()));
        Ok(pending)
    }

    pub async fn pending(
        &self,
        id: Uuid,
    ) -> Result<Option<(PendingCredentialStatus, Candidate)>, AppError> {
        let mut drafts = self.drafts.lock().await;
        drafts.retain(|_, (status, _)| {
            Utc::now() - status.created_at < chrono::Duration::minutes(30)
        });
        Ok(drafts.get(&id).cloned())
    }

    pub async fn claim_draft(&self, id: Uuid, user_id: &str) -> Result<(), AppError> {
        let mut drafts = self.drafts.lock().await;
        let (status, _) = drafts
            .get_mut(&id)
            .ok_or_else(|| AppError::NotFound("validation draft not found".into()))?;
        if status.owner.is_some() {
            return Err(AppError::Conflict(
                "validation draft already claimed".into(),
            ));
        }
        status.owner = Some(user_id.to_owned());
        Ok(())
    }

    pub async fn authorize_draft(&self, id: Uuid, user_id: &str) -> Result<(), AppError> {
        let (status, _) = self
            .pending(id)
            .await?
            .filter(|(status, _)| status.owner.as_deref() == Some(user_id))
            .ok_or_else(|| AppError::NotFound("validation draft not found".into()))?;
        debug_assert_eq!(status.owner.as_deref(), Some(user_id));
        Ok(())
    }

    pub async fn pending_status(
        &self,
        handle: &Handle,
    ) -> Result<Vec<PendingCredentialStatus>, AppError> {
        let mut drafts = self.drafts.lock().await;
        drafts.retain(|_, (status, _)| {
            Utc::now() - status.created_at < chrono::Duration::minutes(30)
        });
        Ok(drafts
            .values()
            .filter(|(status, _)| &status.handle == handle)
            .map(|(status, _)| status.clone())
            .collect())
    }

    pub async fn promote(
        &self,
        id: Uuid,
        expected: Option<u64>,
    ) -> Result<CredentialStatus, AppError> {
        let (pending, candidate) = self
            .drafts
            .lock()
            .await
            .remove(&id)
            .ok_or_else(|| AppError::NotFound("credential draft".into()))?;
        if Utc::now() - pending.created_at >= chrono::Duration::minutes(30)
            || expected.is_some_and(|generation| generation != pending.base_generation)
        {
            return Err(AppError::Conflict(
                "credential draft expired or changed".into(),
            ));
        }
        let integration = integration(pending.method, &pending.binding)?;
        let metadata = serde_json::json!({"name":pending.handle});
        let current_id = pending
            .binding
            .authentication_attributes
            .get("credential_id")
            .and_then(|id| id.as_str())
            .map(Uuid::parse_str)
            .transpose()
            .map_err(internal)?;
        let saved = match candidate {
            Candidate::Secret(payload) => match current_id {
                Some(id) => {
                    let current = self
                        .vault
                        .status_by_id(id)
                        .await?
                        .ok_or_else(|| AppError::NotFound("managed credential".into()))?;
                    self.vault
                        .replace_by_id(id, pending.version, integration, current.metadata, payload)
                        .await?
                }
                None => self.vault.create(integration, metadata, payload).await?,
            },
            Candidate::Existing(version) => self
                .vault
                .status_by_id(
                    current_id
                        .ok_or_else(|| AppError::Validation("credential ID is required".into()))?,
                )
                .await?
                .filter(|status| !status.removed && status.version == version)
                .ok_or_else(|| AppError::Conflict("credential version changed".into()))?,
            Candidate::External => {
                return Err(AppError::Validation(
                    "external credentials are not stored".into(),
                ));
            }
        };
        Ok(CredentialStatus {
            handle: pending.handle,
            method: pending.method,
            state: CredentialState::Active,
            generation: saved.generation,
            version: saved.version,
            item_id: saved.item_id,
            binding: pending.binding,
            updated_at: saved.updated_at,
        })
    }

    pub async fn discard_pending(&self, id: Uuid) -> Result<(), AppError> {
        self.drafts.lock().await.remove(&id);
        Ok(())
    }
}

fn integration(
    method: CredentialMethod,
    binding: &CredentialBinding,
) -> Result<&'static str, AppError> {
    use managed::integration::{copilot, openai_codex, static_secret};

    match (method, binding.provider.as_str()) {
        (CredentialMethod::ApiKey | CredentialMethod::Oauth, "github-copilot") => Ok(copilot::ID),
        (CredentialMethod::Oauth, "openai") => Ok(openai_codex::ID),
        (CredentialMethod::ApiKey, _) => Ok(static_secret::ID),
        // External validation proofs have no stored document.
        (CredentialMethod::Aws | CredentialMethod::AzureEntra | CredentialMethod::Anonymous, _) => {
            Ok(static_secret::ID)
        }
        _ => Err(AppError::Validation(
            "provider credential method is unsupported".into(),
        )),
    }
}

fn internal(error: impl std::fmt::Display) -> AppError {
    AppError::Internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn store() -> (Surreal<Db>, ProviderCredentials) {
        let db = Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store =
            crate::credential::managed::test_support::credentials(db.clone(), "test-secret").await;
        (db, store)
    }

    fn binding(id: Option<Uuid>) -> CredentialBinding {
        let mut authentication_attributes = serde_json::Map::new();
        if let Some(id) = id {
            authentication_attributes.insert("credential_id".into(), id.to_string().into());
        }
        CredentialBinding {
            provider: "openai".into(),
            adapter: AdapterId::Openai,
            effective_endpoint: Some("https://api.openai.com/v1".into()),
            source: "database".into(),
            authentication_attributes,
        }
    }

    async fn draft(
        store: &ProviderCredentials,
        id: Option<Uuid>,
        key: &str,
    ) -> PendingCredentialStatus {
        store
            .stage(
                &Handle::const_validated("account"),
                CredentialMethod::ApiKey,
                binding(id),
                &Candidate::Secret(managed::integration::static_secret::document(key.into())),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn drafts_are_private_expiring_bounded_memory_and_restart_loses_them() {
        let (db, store) = store().await;
        let pending = draft(&store, None, "private-secret").await;
        assert!(
            store
                .authorize_draft(pending.validation_id, "alice")
                .await
                .is_err()
        );
        store
            .claim_draft(pending.validation_id, "alice")
            .await
            .unwrap();
        store
            .authorize_draft(pending.validation_id, "alice")
            .await
            .unwrap();
        assert!(
            store
                .authorize_draft(pending.validation_id, "bob")
                .await
                .is_err()
        );
        assert!(
            store
                .claim_draft(pending.validation_id, "bob")
                .await
                .is_err()
        );
        let restarted =
            crate::credential::managed::test_support::credentials(db.clone(), "test-secret").await;
        assert!(
            restarted
                .pending(pending.validation_id)
                .await
                .unwrap()
                .is_none()
        );
        let mut response = db.query("SELECT * FROM managed_credential").await.unwrap();
        let records: Vec<serde_json::Value> = response.take(0).unwrap();
        assert!(records.is_empty());
        for _ in 1..64 {
            draft(&store, None, "private-secret").await;
        }
        assert!(
            store
                .stage(
                    &Handle::const_validated("account"),
                    CredentialMethod::ApiKey,
                    binding(None),
                    &Candidate::External
                )
                .await
                .is_err()
        );
        store
            .drafts
            .lock()
            .await
            .get_mut(&pending.validation_id)
            .unwrap()
            .0
            .created_at = Utc::now() - chrono::Duration::minutes(31);
        assert!(
            store
                .pending(pending.validation_id)
                .await
                .unwrap()
                .is_none()
        );
        draft(&store, None, "replacement").await;
    }

    #[tokio::test]
    async fn accepting_is_explicit_single_use_and_preserves_the_id_on_replacement() {
        let (_, store) = store().await;
        let first = draft(&store, None, "first").await;
        assert!(store.vault.list().await.unwrap().is_empty());
        let accepted = store.promote(first.validation_id, None).await.unwrap();
        assert!(store.promote(first.validation_id, None).await.is_err());
        let next = draft(&store, Some(accepted.item_id), "next").await;
        let (_, payload) = store
            .active_id(Some(accepted.item_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payload["API_KEY"], "first");
        let updated = store
            .promote(next.validation_id, Some(accepted.generation))
            .await
            .unwrap();
        assert_eq!(updated.item_id, accepted.item_id);
        assert_ne!(updated.version, accepted.version);
        assert_eq!(
            store
                .active_id(Some(updated.item_id))
                .await
                .unwrap()
                .unwrap()
                .1["API_KEY"],
            "next"
        );
    }

    #[tokio::test]
    async fn stale_drafts_cannot_overwrite_replacements_or_recreate_deleted_ids() {
        let (_, store) = store().await;
        let initial = draft(&store, None, "first").await;
        let active = store.promote(initial.validation_id, None).await.unwrap();
        let a = draft(&store, Some(active.item_id), "a").await;
        let b = draft(&store, Some(active.item_id), "b").await;
        let (a, b) = tokio::join!(
            store.promote(a.validation_id, None),
            store.promote(b.validation_id, None)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let updated = a.ok().or_else(|| b.ok()).unwrap();
        let stale = draft(&store, Some(updated.item_id), "stale").await;
        store
            .vault
            .delete_by_id(updated.item_id, updated.version)
            .await
            .unwrap();
        assert!(store.promote(stale.validation_id, None).await.is_err());
        let recreated = draft(&store, None, "fresh").await;
        let recreated = store.promote(recreated.validation_id, None).await.unwrap();
        assert_ne!(recreated.item_id, updated.item_id);
    }

    #[tokio::test]
    async fn cancel_and_expiry_never_modify_the_saved_credential() {
        let (_, store) = store().await;
        let initial = draft(&store, None, "first").await;
        let active = store.promote(initial.validation_id, None).await.unwrap();
        let cancelled = draft(&store, Some(active.item_id), "cancelled").await;
        store
            .discard_pending(cancelled.validation_id)
            .await
            .unwrap();
        assert!(store.promote(cancelled.validation_id, None).await.is_err());
        let expired = draft(&store, Some(active.item_id), "expired").await;
        store
            .drafts
            .lock()
            .await
            .get_mut(&expired.validation_id)
            .unwrap()
            .0
            .created_at = Utc::now() - chrono::Duration::minutes(31);
        assert!(store.promote(expired.validation_id, None).await.is_err());
        assert_eq!(
            store
                .active_id(Some(active.item_id))
                .await
                .unwrap()
                .unwrap()
                .1["API_KEY"],
            "first"
        );
    }

    // Reproducer for the user-deferred key-lifetime issue, not a security acceptance test.
    #[tokio::test]
    async fn database_alone_contains_the_key_needed_to_decrypt_provider_credentials() {
        let (db, writer) = store().await;
        crate::credential::key_rotation::KeyRotation::check(&db, "test-secret")
            .await
            .unwrap();
        let pending = draft(&writer, None, "synthetic-provider-key").await;
        let saved = writer.promote(pending.validation_id, None).await.unwrap();
        drop(writer);
        let mut response = db
            .query("SELECT VALUE `value` FROM runtime_config WHERE `key` = 'encryption_secret' LIMIT 1")
            .await
            .unwrap();
        let recovered: Option<String> = response.take(0).unwrap();
        let reader =
            crate::credential::managed::test_support::credentials(db, &recovered.unwrap()).await;
        assert_eq!(
            reader
                .active_id(Some(saved.item_id))
                .await
                .unwrap()
                .unwrap()
                .1["API_KEY"],
            "synthetic-provider-key"
        );
    }

    #[tokio::test]
    async fn external_proofs_are_not_persisted_as_credentials() {
        let (_, store) = store().await;
        let pending = store
            .stage(
                &Handle::const_validated("account"),
                CredentialMethod::ApiKey,
                binding(None),
                &Candidate::External,
            )
            .await
            .unwrap();
        assert!(store.promote(pending.validation_id, None).await.is_err());
        assert!(store.vault.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn saved_secrets_are_encrypted_and_rotation_does_not_persist_drafts() {
        let (db, store) = store().await;
        crate::credential::key_rotation::KeyRotation::check(&db, "test-secret")
            .await
            .unwrap();
        let initial = draft(&store, None, "saved-secret").await;
        let active = store.promote(initial.validation_id, None).await.unwrap();
        let pending = draft(&store, Some(active.item_id), "pending-secret").await;
        let mut response = db.query("SELECT * FROM managed_credential").await.unwrap();
        let records: Vec<serde_json::Value> = response.take(0).unwrap();
        assert!(!format!("{records:?}").contains("saved-secret"));
        assert!(!format!("{records:?}").contains("pending-secret"));
        let rotation = crate::credential::key_rotation::KeyRotation::check(&db, "rotated-secret")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rotation.run().await.unwrap().managed_credentials.success, 1);
        let restarted =
            crate::credential::managed::test_support::credentials(db, "rotated-secret").await;
        assert_eq!(
            restarted
                .active_id(Some(active.item_id))
                .await
                .unwrap()
                .unwrap()
                .1,
            json!({"API_KEY":"saved-secret"})
        );
        assert!(
            restarted
                .pending(pending.validation_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
