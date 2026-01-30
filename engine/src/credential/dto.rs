use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::models::{Credential, CredentialData};

#[derive(Debug, Deserialize)]
pub struct CreateCredentialRequest {
    pub name: String,
    pub provider: String,
    pub data: CredentialData,
}

#[derive(Debug, Serialize)]
pub struct CredentialResponse {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub data: CredentialResponseData,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum CredentialResponseData {
    BrowserProfile,
    UsernamePassword { username: String },
}

impl From<Credential> for CredentialResponse {
    fn from(c: Credential) -> Self {
        let data = match &c.data {
            CredentialData::BrowserProfile => CredentialResponseData::BrowserProfile,
            CredentialData::UsernamePassword { username, .. } => {
                CredentialResponseData::UsernamePassword {
                    username: username.clone(),
                }
            }
        };
        Self {
            id: c.id,
            name: c.name,
            provider: c.provider,
            data,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}
