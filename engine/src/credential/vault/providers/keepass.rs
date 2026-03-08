use std::collections::HashMap;
use std::fs::File;

use async_trait::async_trait;
use keepass::{Database, DatabaseKey};

use crate::core::error::AppError;
use crate::credential::vault::models::{VaultItem, VaultSecret};
use crate::credential::vault::provider::VaultProvider;

pub struct KeePassVaultProvider {
    file_path: String,
    master_password: String,
}

impl KeePassVaultProvider {
    pub fn new(file_path: String, master_password: String) -> Self {
        Self {
            file_path,
            master_password,
        }
    }

    fn open_db(&self) -> Result<Database, AppError> {
        let mut file = File::open(&self.file_path)
            .map_err(|e| AppError::Tool(format!("Failed to open KeePass file '{}': {e}", self.file_path)))?;

        let key = DatabaseKey::new().with_password(&self.master_password);

        Database::open(&mut file, key)
            .map_err(|e| AppError::Tool(format!("Failed to unlock KeePass database: {e}")))
    }
}

fn collect_entries(group: &keepass::db::Group, results: &mut Vec<(String, keepass::db::Entry)>) {
    for entry in &group.entries {
        let uuid_hex = format!("{:032x}", entry.uuid.as_u128());
        results.push((uuid_hex, entry.clone()));
    }
    for child_group in &group.groups {
        collect_entries(child_group, results);
    }
}

#[async_trait]
impl VaultProvider for KeePassVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let file_path = self.file_path.clone();
        let master_password = self.master_password.clone();
        let query = query.to_string();

        tokio::task::spawn_blocking(move || {
            let provider = KeePassVaultProvider::new(file_path, master_password);
            let db = provider.open_db()?;
            let query_lower = query.to_lowercase();

            let mut all_entries = Vec::new();
            collect_entries(&db.root, &mut all_entries);

            let items: Vec<VaultItem> = all_entries
                .iter()
                .filter(|(_, entry)| {
                    let title_match = entry
                        .get_title()
                        .map(|t| t.to_lowercase().contains(&query_lower))
                        .unwrap_or(false);
                    let username_match = entry
                        .get_username()
                        .map(|u| u.to_lowercase().contains(&query_lower))
                        .unwrap_or(false);
                    title_match || username_match
                })
                .take(max_results)
                .map(|(uuid_hex, entry)| VaultItem {
                    id: uuid_hex.clone(),
                    name: entry.get_title().unwrap_or("Untitled").to_string(),
                    username: entry.get_username().map(|s| s.to_string()),
                })
                .collect();

            Ok(items)
        })
        .await
        .map_err(|e| AppError::Tool(format!("KeePass search task failed: {e}")))?
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let file_path = self.file_path.clone();
        let master_password = self.master_password.clone();
        let item_id = item_id.to_string();

        tokio::task::spawn_blocking(move || {
            let provider = KeePassVaultProvider::new(file_path, master_password);
            let db = provider.open_db()?;

            let mut all_entries = Vec::new();
            collect_entries(&db.root, &mut all_entries);

            let (_, entry) = all_entries
                .iter()
                .find(|(uuid_hex, _)| uuid_hex == &item_id)
                .ok_or_else(|| AppError::Tool(format!("KeePass entry not found: {item_id}")))?;

            let mut fields = HashMap::new();

            if let Some(url) = entry.get_url()
                && !url.is_empty()
            {
                fields.insert("url".to_string(), url.to_string());
            }

            let standard_keys = ["Title", "UserName", "Password", "URL", "Notes"];
            for (key, value) in &entry.fields {
                if !standard_keys.contains(&key.as_str()) {
                    let v = value.get();
                    if !v.is_empty() {
                        fields.insert(key.clone(), v.clone());
                    }
                }
            }

            Ok(VaultSecret {
                id: item_id,
                name: entry.get_title().unwrap_or("Untitled").to_string(),
                username: entry.get_username().map(|s| s.to_string()),
                password: entry.get_password().map(|s| s.to_string()),
                notes: entry.get("Notes").map(|s| s.to_string()),
                fields,
            })
        })
        .await
        .map_err(|e| AppError::Tool(format!("KeePass get_secret task failed: {e}")))?
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        let file_path = self.file_path.clone();
        let master_password = self.master_password.clone();

        tokio::task::spawn_blocking(move || {
            let provider = KeePassVaultProvider::new(file_path, master_password);
            provider.open_db()?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Tool(format!("KeePass test_connection task failed: {e}")))?
    }
}
