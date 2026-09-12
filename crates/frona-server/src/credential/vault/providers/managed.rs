use super::super::{
    models::{VaultConnection, VaultItem, VaultSecret},
    provider::VaultProvider,
    repository::VaultConnectionRepository,
};
use crate::{
    core::error::AppError,
    credential::managed::{ManagedVault, resolver::ManagedResolver},
};
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

pub struct ManagedVaultProvider {
    pub(crate) vault: ManagedVault,
    pub(crate) resolver: Arc<ManagedResolver>,
    pub(crate) connection: VaultConnection,
    pub(crate) connections: Arc<dyn VaultConnectionRepository>,
    pub(crate) user_id: String,
}

impl ManagedVaultProvider {
    async fn authorize(&self) -> Result<(), AppError> {
        let current = self
            .connections
            .find_by_id(&self.connection.id)
            .await?
            .ok_or_else(|| AppError::NotFound("vault connection".into()))?;
        if !current.enabled || (!current.system_managed && current.user_id != self.user_id) {
            return Err(AppError::Forbidden(
                "vault connection is unavailable".into(),
            ));
        }
        if current.provider != self.connection.provider
            || current.config_encrypted != self.connection.config_encrypted
            || current.nonce != self.connection.nonce
        {
            return Err(AppError::Conflict(
                "vault connection changed during resolution".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl VaultProvider for ManagedVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        self.authorize().await?;
        let query = query.to_lowercase();
        Ok(self
            .vault
            .list()
            .await?
            .into_iter()
            .filter(|s| !s.removed)
            .map(|s| VaultItem {
                id: s.item_id.to_string(),
                name: s
                    .metadata
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{} / {}", s.key.connection, s.key.slot)),
                username: None,
            })
            .filter(|item| item.name.to_lowercase().contains(&query))
            .take(max_results)
            .collect())
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let id = Uuid::parse_str(item_id)
            .map_err(|_| AppError::Validation("invalid managed credential ID".into()))?;
        let (status, secret) = self
            .resolver
            .resolve(&self.vault, id, || self.authorize())
            .await?;
        Ok(VaultSecret {
            id: id.to_string(),
            name: status
                .metadata
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} / {}", status.key.connection, status.key.slot)),
            username: None,
            password: None,
            notes: None,
            fields: secret.to_env()?,
        })
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        self.authorize().await?;
        self.vault.list().await?;
        Ok(())
    }
}
