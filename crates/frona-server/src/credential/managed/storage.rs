use super::repository::ManagedVaultRepository;
use super::*;
use crate::core::error::AppError;
use crate::credential::key_rotation::derive_key;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use std::sync::Arc;

#[derive(Clone)]
pub struct ManagedVault {
    repo: Arc<dyn ManagedVaultRepository>,
    encryption_key: [u8; 32],
    connection_id: String,
}

impl ManagedVault {
    pub fn new(repo: Arc<dyn ManagedVaultRepository>, secret: &str, connection_id: String) -> Self {
        Self {
            repo,
            encryption_key: derive_key(secret),
            connection_id,
        }
    }

    pub(crate) fn for_connection(&self, connection_id: String) -> Self {
        Self {
            connection_id,
            ..self.clone()
        }
    }

    pub(crate) fn connection_id(&self) -> &str {
        &self.connection_id
    }

    pub(crate) async fn ensure_available(&self) -> Result<(), AppError> {
        self.repo.ensure_available(&self.connection_id).await
    }

    pub(crate) async fn delete_connection(&self, user_id: &str) -> Result<(), AppError> {
        self.repo
            .delete_connection(&self.connection_id, user_id)
            .await
    }

    pub async fn status_by_id(&self, id: Uuid) -> Result<Option<Status>, AppError> {
        self.ensure_available().await?;
        self.repo
            .row_by_id(&self.connection_id, id)
            .await?
            .map(|row| serde_json::from_value(row).map_err(internal))
            .transpose()
    }

    /// Persist an accepted secret independently of provider configuration.
    pub async fn create(
        &self,
        integration: &str,
        metadata: Value,
        payload: Value,
    ) -> Result<Status, AppError> {
        let id = Uuid::new_v4();
        let status = Status {
            integration: integration.to_owned(),
            key: Key::new(id.to_string(), "credential"),
            item_id: id,
            generation: 1,
            version: Uuid::new_v4(),
            deleted: false,
            removed: false,
            metadata,
            updated_at: Utc::now(),
        };
        self.write_secret(status, payload, None).await
    }

    pub async fn replace_by_id(
        &self,
        id: Uuid,
        expected_version: Uuid,
        integration: &str,
        metadata: Value,
        payload: Value,
    ) -> Result<Status, AppError> {
        let mut current = self
            .status_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound("managed credential".into()))?;
        if current.version != expected_version {
            return Err(AppError::Conflict("credential version changed".into()));
        }
        current.generation = current
            .generation
            .checked_add(1)
            .ok_or_else(|| AppError::Internal("credential generation overflowed".into()))?;
        current.version = Uuid::new_v4();
        current.integration = integration.to_owned();
        current.metadata = metadata;
        current.removed = false;
        current.updated_at = Utc::now();
        self.write_secret(current, payload, Some(expected_version))
            .await
    }

    pub async fn delete_by_id(&self, id: Uuid, expected_version: Uuid) -> Result<(), AppError> {
        self.ensure_available().await?;
        self.repo
            .delete_by_id(&self.connection_id, id, expected_version)
            .await
    }

    async fn write_secret(
        &self,
        status: Status,
        payload: Value,
        expected_version: Option<Uuid>,
    ) -> Result<Status, AppError> {
        self.ensure_available().await?;
        if status.integration.is_empty() {
            return Err(AppError::Validation(
                "credential integration must not be empty".into(),
            ));
        }
        let (ciphertext, nonce) = self.encrypt(&payload)?;
        let mut row = serde_json::to_value(&status).map_err(internal)?;
        row["ciphertext"] = serde_json::to_value(ciphertext).map_err(internal)?;
        row["nonce"] = serde_json::to_value(nonce).map_err(internal)?;
        self.repo
            .write(&self.connection_id, row, expected_version)
            .await?;
        Ok(status)
    }

    pub(crate) async fn document_by_id(&self, id: Uuid) -> Result<(Status, Value), AppError> {
        let row = self
            .repo
            .row_by_id(&self.connection_id, id)
            .await?
            .ok_or_else(|| AppError::NotFound("managed credential".into()))?;
        let status: Status = serde_json::from_value(row.clone()).map_err(internal)?;
        if status.removed {
            return Err(AppError::Validation(
                "managed credential is logged out".into(),
            ));
        }
        Ok((status, self.decrypt(&row)?))
    }

    pub async fn list(&self) -> Result<Vec<Status>, AppError> {
        self.ensure_available().await?;
        self.repo.list(&self.connection_id).await
    }

    fn encrypt(&self, payload: &Value) -> Result<(Vec<u8>, Vec<u8>), AppError> {
        let plaintext = serde_json::to_vec(payload).map_err(internal)?;
        let nonce: [u8; 12] = rand::random();
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key).map_err(internal)?;
        let ciphertext = cipher
            .encrypt(&Nonce::from(nonce), plaintext.as_ref())
            .map_err(|_| AppError::Internal("credential encryption failed".into()))?;
        Ok((ciphertext, nonce.to_vec()))
    }

    fn decrypt(&self, row: &Value) -> Result<Value, AppError> {
        let ciphertext: Vec<u8> =
            serde_json::from_value(row["ciphertext"].clone()).map_err(internal)?;
        let nonce: [u8; 12] = serde_json::from_value(row["nonce"].clone()).map_err(internal)?;
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key).map_err(internal)?;
        let plaintext = cipher
            .decrypt(&Nonce::from(nonce), ciphertext.as_ref())
            .map_err(|_| AppError::Internal("credential decryption failed".into()))?;
        serde_json::from_slice(&plaintext).map_err(internal)
    }
}

fn internal(error: impl std::fmt::Display) -> AppError {
    AppError::Internal(error.to_string())
}
