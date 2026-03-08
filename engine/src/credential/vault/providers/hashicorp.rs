use std::collections::HashMap;

use async_trait::async_trait;
use vaultrs::client::{VaultClient, VaultClientSettingsBuilder};
use vaultrs::kv2;

use crate::core::error::AppError;
use crate::credential::vault::models::{VaultItem, VaultSecret};
use crate::credential::vault::provider::VaultProvider;

pub struct HashicorpVaultProvider {
    client: VaultClient,
    mount: String,
}

impl HashicorpVaultProvider {
    pub fn new(address: String, token: String, mount_path: Option<String>) -> Result<Self, AppError> {
        let settings = VaultClientSettingsBuilder::default()
            .address(&address)
            .token(&token)
            .build()
            .map_err(|e| AppError::Tool(format!("Failed to build HashiCorp Vault client settings: {e}")))?;

        let client = VaultClient::new(settings)
            .map_err(|e| AppError::Tool(format!("Failed to create HashiCorp Vault client: {e}")))?;

        Ok(Self {
            client,
            mount: mount_path.unwrap_or_else(|| "secret".to_string()),
        })
    }

    async fn list_keys(&self) -> Result<Vec<String>, AppError> {
        kv2::list(&self.client, &self.mount, "")
            .await
            .map_err(|e| AppError::Tool(format!("Failed to list keys from HashiCorp Vault: {e}")))
    }
}

#[async_trait]
impl VaultProvider for HashicorpVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let keys = self.list_keys().await?;
        let query_lower = query.to_lowercase();

        let items: Vec<VaultItem> = keys
            .into_iter()
            .filter(|key| key.to_lowercase().contains(&query_lower))
            .take(max_results)
            .map(|key| VaultItem {
                id: key.clone(),
                name: key,
                username: None,
            })
            .collect();

        Ok(items)
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let data: HashMap<String, String> = kv2::read(&self.client, &self.mount, item_id)
            .await
            .map_err(|e| AppError::Tool(format!("Failed to read secret '{item_id}' from HashiCorp Vault: {e}")))?;

        let username = data.get("username").cloned();
        let password = data.get("password").cloned();
        let notes = data.get("notes").cloned();

        let fields: HashMap<String, String> = data
            .into_iter()
            .filter(|(k, _)| k != "username" && k != "password" && k != "notes")
            .collect();

        Ok(VaultSecret {
            id: item_id.to_string(),
            name: item_id.to_string(),
            username,
            password,
            notes,
            fields,
        })
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        self.list_keys().await?;
        Ok(())
    }
}
