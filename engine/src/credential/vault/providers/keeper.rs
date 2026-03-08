use std::collections::HashMap;

use async_trait::async_trait;
use keeper_secrets_manager_core::core::{ClientOptions, SecretsManager};
use keeper_secrets_manager_core::enums::KvStoreType;
use keeper_secrets_manager_core::storage::InMemoryKeyValueStorage;

use crate::core::error::AppError;
use crate::credential::vault::models::{VaultItem, VaultSecret};
use crate::credential::vault::provider::VaultProvider;

pub struct KeeperVaultProvider {
    token: String,
    server: Option<String>,
}

impl KeeperVaultProvider {
    pub fn new(token: String, server: Option<String>) -> Self {
        Self { token, server }
    }

    fn create_secrets_manager(token: &str, server: &Option<String>) -> Result<SecretsManager, AppError> {
        let storage = InMemoryKeyValueStorage::new(None)
            .map_err(|e| AppError::Tool(format!("Failed to create Keeper storage: {e}")))?;
        let config = KvStoreType::InMemory(storage);
        let mut options = ClientOptions::new_client_options_with_token(token.to_string(), config);
        if let Some(srv) = server {
            options.hostname = Some(srv.clone());
        }
        SecretsManager::new(options)
            .map_err(|e| AppError::Tool(format!("Failed to create Keeper client: {e}")))
    }
}

#[async_trait]
impl VaultProvider for KeeperVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let token = self.token.clone();
        let server = self.server.clone();
        let query = query.to_lowercase();

        tokio::task::spawn_blocking(move || {
            let mut sm = Self::create_secrets_manager(&token, &server)?;
            let records = sm
                .get_secrets(Vec::new())
                .map_err(|e| AppError::Tool(format!("Failed to fetch Keeper secrets: {e}")))?;

            let items: Vec<VaultItem> = records
                .iter()
                .filter(|r| r.title.to_lowercase().contains(&query))
                .take(max_results)
                .map(|r| {
                    let username = r
                        .get_standard_field_value("login", true)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from));
                    VaultItem {
                        id: r.uid.clone(),
                        name: r.title.clone(),
                        username,
                    }
                })
                .collect();

            Ok(items)
        })
        .await
        .map_err(|e| AppError::Tool(format!("Keeper task failed: {e}")))?
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let token = self.token.clone();
        let server = self.server.clone();
        let item_id = item_id.to_string();

        tokio::task::spawn_blocking(move || {
            let mut sm = Self::create_secrets_manager(&token, &server)?;
            let records = sm
                .get_secrets(vec![item_id.clone()])
                .map_err(|e| AppError::Tool(format!("Failed to fetch Keeper secret: {e}")))?;

            let record = records
                .first()
                .ok_or_else(|| AppError::Tool(format!("Keeper secret '{item_id}' not found")))?;

            let username = record
                .get_standard_field_value("login", true)
                .ok()
                .and_then(|v| v.as_str().map(String::from));
            let password = record.password.clone();
            let notes = record
                .get_standard_field_value("note", true)
                .ok()
                .and_then(|v| v.as_str().map(String::from));

            let mut fields = HashMap::new();
            if let Ok(url) = record.get_standard_field_value("url", true)
                && let Some(u) = url.as_str()
            {
                fields.insert("url".to_string(), u.to_string());
            }

            Ok(VaultSecret {
                id: item_id,
                name: record.title.clone(),
                username,
                password,
                notes,
                fields,
            })
        })
        .await
        .map_err(|e| AppError::Tool(format!("Keeper task failed: {e}")))?
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        let token = self.token.clone();
        let server = self.server.clone();

        tokio::task::spawn_blocking(move || {
            let mut sm = Self::create_secrets_manager(&token, &server)?;
            sm.get_secrets(Vec::new())
                .map_err(|e| AppError::Tool(format!("Keeper connection test failed: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Tool(format!("Keeper task failed: {e}")))?
    }
}
