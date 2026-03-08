use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::core::error::AppError;
use crate::credential::vault::models::{VaultItem, VaultSecret};
use crate::credential::vault::provider::VaultProvider;

pub struct OnePasswordVaultProvider {
    client: reqwest::Client,
    host: String,
    default_vault_id: Option<String>,
}

#[derive(Deserialize)]
struct OpVault {
    id: String,
}

#[derive(Deserialize)]
struct OpItem {
    id: String,
    title: String,
    vault: OpVault,
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
    pub fn new(connect_host: String, connect_token: String, default_vault_id: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {connect_token}").parse().unwrap(),
                );
                headers
            })
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            host: connect_host.trim_end_matches('/').to_string(),
            default_vault_id,
        }
    }

    async fn get_vault_ids(&self) -> Result<Vec<String>, AppError> {
        if let Some(ref id) = self.default_vault_id {
            return Ok(vec![id.clone()]);
        }

        let resp = self
            .client
            .get(format!("{}/v1/vaults", self.host))
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("1Password Connect request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(AppError::Tool(format!(
                "1Password Connect API error: {}",
                resp.status()
            )));
        }

        let vaults: Vec<OpVault> = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("1Password response parse failed: {e}")))?;

        Ok(vaults.into_iter().map(|v| v.id).collect())
    }
}

#[async_trait]
impl VaultProvider for OnePasswordVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let vault_ids = self.get_vault_ids().await?;
        let mut results = Vec::new();

        for vault_id in vault_ids {
            let resp = self
                .client
                .get(format!("{}/v1/vaults/{vault_id}/items", self.host))
                .query(&[("filter", format!("title co \"{query}\""))])
                .send()
                .await
                .map_err(|e| AppError::Tool(format!("1Password search failed: {e}")))?;

            if !resp.status().is_success() {
                continue;
            }

            let items: Vec<OpItem> = resp
                .json()
                .await
                .map_err(|e| AppError::Tool(format!("1Password response parse failed: {e}")))?;

            for item in items {
                results.push(VaultItem {
                    id: format!("{}:{}", item.vault.id, item.id),
                    name: item.title,
                    username: None,
                });
                if results.len() >= max_results {
                    return Ok(results);
                }
            }
        }

        Ok(results)
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let (vault_id, op_item_id) = item_id
            .split_once(':')
            .ok_or_else(|| AppError::Tool("Invalid 1Password item ID format (expected vault_id:item_id)".into()))?;

        let resp = self
            .client
            .get(format!("{}/v1/vaults/{vault_id}/items/{op_item_id}", self.host))
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("1Password get secret failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(AppError::Tool(format!(
                "1Password API error: {}",
                resp.status()
            )));
        }

        let detail: OpItemDetail = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("1Password response parse failed: {e}")))?;

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
        let resp = self
            .client
            .get(format!("{}/v1/vaults", self.host))
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("1Password Connect request failed: {e}")))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(AppError::Tool(format!(
                "1Password Connect test failed: {}",
                resp.status()
            )))
        }
    }
}
