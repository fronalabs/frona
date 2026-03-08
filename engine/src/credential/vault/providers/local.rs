use std::sync::Arc;

use async_trait::async_trait;

use crate::core::error::AppError;
use crate::credential::vault::models::{Credential, CredentialData, VaultItem, VaultSecret};
use crate::credential::vault::repository::CredentialRepository;
use crate::credential::vault::provider::VaultProvider;

pub struct LocalVaultProvider {
    repo: Arc<dyn CredentialRepository>,
    encryption_key: [u8; 32],
    user_id: String,
}

impl LocalVaultProvider {
    pub fn new(
        repo: Arc<dyn CredentialRepository>,
        encryption_key: [u8; 32],
        user_id: String,
    ) -> Self {
        Self {
            repo,
            encryption_key,
            user_id,
        }
    }

    fn credential_to_item(c: &Credential) -> VaultItem {
        let username = match &c.data {
            CredentialData::UsernamePassword { username, .. } => Some(username.clone()),
            CredentialData::BrowserProfile | CredentialData::ApiKey { .. } => None,
        };
        VaultItem {
            id: c.id.clone(),
            name: format!("{} ({})", c.name, c.provider),
            username,
        }
    }

    fn credential_to_secret(&self, c: &Credential) -> Result<VaultSecret, AppError> {
        match &c.data {
            CredentialData::UsernamePassword {
                username,
                password_encrypted,
            } => {
                let password = crate::credential::vault::service::decrypt_password(
                    password_encrypted,
                    &self.encryption_key,
                )?;
                Ok(VaultSecret {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    username: Some(username.clone()),
                    password: Some(password),
                    notes: None,
                    fields: std::collections::HashMap::new(),
                })
            }
            CredentialData::ApiKey { key_encrypted } => {
                let api_key = crate::credential::vault::service::decrypt_password(
                    key_encrypted,
                    &self.encryption_key,
                )?;
                let mut fields = std::collections::HashMap::new();
                fields.insert("API_KEY".to_string(), api_key);
                Ok(VaultSecret {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    username: None,
                    password: None,
                    notes: None,
                    fields,
                })
            }
            CredentialData::BrowserProfile => Ok(VaultSecret {
                id: c.id.clone(),
                name: c.name.clone(),
                username: None,
                password: None,
                notes: None,
                fields: std::collections::HashMap::new(),
            }),
        }
    }
}

#[async_trait]
impl VaultProvider for LocalVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let all = self.repo.find_by_user_id(&self.user_id).await?;
        let query_lower = query.to_lowercase();
        let items: Vec<VaultItem> = all
            .iter()
            .filter(|c| {
                c.name.to_lowercase().contains(&query_lower)
                    || c.provider.to_lowercase().contains(&query_lower)
            })
            .take(max_results)
            .map(Self::credential_to_item)
            .collect();
        Ok(items)
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let credential = self
            .repo
            .find_by_id(item_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Credential not found".into()))?;

        if credential.user_id != self.user_id {
            return Err(AppError::Forbidden("Not your credential".into()));
        }

        self.credential_to_secret(&credential)
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        Ok(())
    }
}
