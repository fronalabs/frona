use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::core::error::AppError;
use crate::credential::vault::models::{VaultItem, VaultSecret};
use crate::credential::vault::provider::VaultProvider;

pub struct BitwardenVaultProvider {
    client: reqwest::Client,
    api_url: String,
    identity_url: String,
    access_token: String,
    organization_id: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct SecretIdentifiersResponse {
    data: Vec<SecretIdentifier>,
}

#[derive(Deserialize)]
struct SecretIdentifier {
    id: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SecretResponse {
    id: String,
    key: String,
    value: String,
    note: Option<String>,
}

impl BitwardenVaultProvider {
    pub fn new(
        access_token: String,
        organization_id: Option<String>,
        api_url: Option<String>,
        identity_url: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_url: api_url.unwrap_or_else(|| "https://api.bitwarden.com".to_string()),
            identity_url: identity_url.unwrap_or_else(|| "https://identity.bitwarden.com".to_string()),
            access_token,
            organization_id,
        }
    }

    async fn get_bearer_token(&self) -> Result<String, AppError> {
        let resp = self
            .client
            .post(format!("{}/connect/token", self.identity_url))
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", "api.secrets"),
                ("client_id", &self.access_token),
                ("client_secret", &self.access_token),
            ])
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("Bitwarden auth request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(AppError::Tool(format!(
                "Bitwarden auth failed: {}",
                resp.status()
            )));
        }

        let token: TokenResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("Bitwarden auth response parse failed: {e}")))?;

        Ok(token.access_token)
    }
}

#[async_trait]
impl VaultProvider for BitwardenVaultProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<VaultItem>, AppError> {
        let bearer = self.get_bearer_token().await?;

        let mut url = format!("{}/secrets", self.api_url);
        if let Some(ref org_id) = self.organization_id {
            url = format!("{}/organizations/{org_id}/secrets", self.api_url);
        }

        let resp = self
            .client
            .get(&url)
            .bearer_auth(&bearer)
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("Bitwarden list secrets failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(AppError::Tool(format!(
                "Bitwarden API error: {}",
                resp.status()
            )));
        }

        let identifiers: SecretIdentifiersResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("Bitwarden response parse failed: {e}")))?;

        let query_lower = query.to_lowercase();
        let results: Vec<VaultItem> = identifiers
            .data
            .into_iter()
            .filter(|s| s.key.to_lowercase().contains(&query_lower))
            .take(max_results)
            .map(|s| VaultItem {
                id: s.id,
                name: s.key,
                username: None,
            })
            .collect();

        Ok(results)
    }

    async fn get_secret(&self, item_id: &str) -> Result<VaultSecret, AppError> {
        let bearer = self.get_bearer_token().await?;

        let resp = self
            .client
            .get(format!("{}/secrets/{item_id}", self.api_url))
            .bearer_auth(&bearer)
            .send()
            .await
            .map_err(|e| AppError::Tool(format!("Bitwarden get secret failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(AppError::Tool(format!(
                "Bitwarden API error: {}",
                resp.status()
            )));
        }

        let secret: SecretResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Tool(format!("Bitwarden response parse failed: {e}")))?;

        let mut fields = HashMap::new();
        fields.insert("value".to_string(), secret.value);

        Ok(VaultSecret {
            id: secret.id,
            name: secret.key,
            username: None,
            password: None,
            notes: secret.note,
            fields,
        })
    }

    async fn test_connection(&self) -> Result<(), AppError> {
        self.get_bearer_token().await?;
        Ok(())
    }
}
