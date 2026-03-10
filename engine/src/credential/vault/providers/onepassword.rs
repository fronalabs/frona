use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

use crate::core::error::AppError;
use crate::credential::vault::models::{VaultItem, VaultSecret};
use crate::credential::vault::provider::VaultProvider;

pub struct OnePasswordVaultProvider {
    service_account_token: String,
    default_vault_id: Option<String>,
    home_dir: PathBuf,
}

#[derive(Deserialize)]
struct OpItem {
    id: String,
    title: String,
    vault: OpVault,
}

#[derive(Deserialize)]
struct OpVault {
    id: String,
}

#[derive(Deserialize)]
struct OpItemDetail {
    title: String,
    #[serde(default)]
    fields: Vec<OpField>,
}

#[derive(Deserialize)]
struct OpField {
    label: Option<String>,
    value: Option<String>,
    purpose: Option<String>,
}

impl OnePasswordVaultProvider {
    pub fn new(service_account_token: String, default_vault_id: Option<String>, home_dir: PathBuf) -> Self {
        Self {
            service_account_token,
            default_vault_id,
            home_dir,
        }
    }

    async fn run_op(&self, args: &[&str]) -> Result<String, AppError> {
        let output = Command::new("op")
            .args(args)
            .arg("--format=json")
            .env("OP_SERVICE_ACCOUNT_TOKEN", &self.service_account_token)
            .env("HOME", &self.home_dir)
            .output()
            .await
            .map_err(|e| AppError::Tool(format!("Failed to run `op` CLI: {e}. Is the 1Password CLI installed?")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not found") || stderr.contains("isn't an item") {
                return Err(AppError::NotFound(format!("1Password item not found: {stderr}")));
            }
            return Err(AppError::Tool(format!("1Password CLI error: {stderr}")));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| AppError::Tool(format!("1Password CLI output not valid UTF-8: {e}")))
    }
}

#[async_trait]
impl VaultProvider for OnePasswordVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let mut args = vec!["item", "list"];
        if let Some(ref vault_id) = self.default_vault_id {
            args.push("--vault");
            args.push(vault_id);
        }

        let json = self.run_op(&args).await?;
        let items: Vec<OpItem> = serde_json::from_str(&json)
            .map_err(|e| AppError::Tool(format!("Failed to parse 1Password item list: {e}")))?;

        let query_lower = query.to_lowercase();
        let results: Vec<VaultItem> = items
            .into_iter()
            .filter(|item| query.is_empty() || item.title.to_lowercase().contains(&query_lower))
            .take(max_results)
            .map(|item| VaultItem {
                id: format!("{}:{}", item.vault.id, item.id),
                name: item.title,
                username: None,
            })
            .collect();

        Ok(results)
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let (vault_id, op_item_id) = item_id
            .split_once(':')
            .ok_or_else(|| AppError::Tool("Invalid 1Password item ID format (expected vault_id:item_id)".into()))?;

        let json = self.run_op(&["item", "get", op_item_id, "--vault", vault_id]).await?;
        let detail: OpItemDetail = serde_json::from_str(&json)
            .map_err(|e| AppError::Tool(format!("Failed to parse 1Password item detail: {e}")))?;

        let mut username = None;
        let mut password = None;
        let mut fields = HashMap::new();

        for field in &detail.fields {
            let value = match &field.value {
                Some(v) if !v.is_empty() => v.clone(),
                _ => continue,
            };

            match field.purpose.as_deref() {
                Some("USERNAME") => username = Some(value),
                Some("PASSWORD") => password = Some(value),
                _ => {
                    if let Some(ref label) = field.label
                        && !label.is_empty()
                    {
                        fields.insert(label.clone(), value);
                    }
                }
            }
        }

        Ok(VaultSecret {
            id: item_id.to_string(),
            name: detail.title,
            username,
            password,
            notes: None,
            fields,
        })
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        let output = Command::new("op")
            .args(["whoami", "--format=json"])
            .env("OP_SERVICE_ACCOUNT_TOKEN", &self.service_account_token)
            .env("HOME", &self.home_dir)
            .output()
            .await
            .map_err(|e| AppError::Tool(format!("Failed to run `op` CLI: {e}. Is the 1Password CLI installed?")))?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(AppError::Tool(format!("1Password CLI auth failed: {stderr}")))
        }
    }
}
