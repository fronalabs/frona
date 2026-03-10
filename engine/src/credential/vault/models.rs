use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::Entity;

// --- Credential types (merged from credential/models.rs) ---

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[serde(tag = "type", content = "data")]
#[surreal(crate = "surrealdb::types", tag = "type", content = "data")]
pub enum CredentialData {
    BrowserProfile,
    UsernamePassword {
        username: String,
        password_encrypted: String,
    },
    ApiKey {
        key_encrypted: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "credential")]
pub struct Credential {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub provider: String,
    pub data: CredentialData,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
    ApiKey,
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
            CredentialData::ApiKey { .. } => CredentialResponseData::ApiKey,
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

// --- Local item request ---

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum CreateLocalItemRequest {
    UsernamePassword {
        name: String,
        username: String,
        password: String,
    },
    ApiKey {
        name: String,
        api_key: String,
    },
    BrowserProfile {
        name: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum UpdateLocalItemRequest {
    UsernamePassword {
        name: String,
        username: String,
        #[serde(default)]
        password: Option<String>,
    },
    ApiKey {
        name: String,
        #[serde(default)]
        api_key: Option<String>,
    },
    BrowserProfile {
        name: String,
    },
}

// --- Vault access log ---

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "vault_access_log")]
pub struct VaultAccessLog {
    pub id: String,
    pub user_id: String,
    pub agent_id: String,
    pub chat_id: String,
    pub connection_id: String,
    pub vault_item_id: String,
    pub env_var_prefix: Option<String>,
    pub query: String,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

// --- Vault types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "snake_case")]
#[surreal(crate = "surrealdb::types")]
pub enum VaultProviderType {
    Local,
    OnePassword,
    Bitwarden,
    Hashicorp,
    KeePass,
    Keeper,
}

impl std::fmt::Display for VaultProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::OnePassword => write!(f, "one_password"),
            Self::Bitwarden => write!(f, "bitwarden"),
            Self::Hashicorp => write!(f, "hashicorp"),
            Self::KeePass => write!(f, "keepass"),
            Self::Keeper => write!(f, "keeper"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VaultConnectionConfig {
    OnePassword {
        service_account_token: String,
        default_vault_id: Option<String>,
    },
    Bitwarden {
        client_id: String,
        client_secret: String,
        master_password: String,
        server_url: Option<String>,
    },
    Hashicorp {
        address: String,
        token: String,
        mount_path: Option<String>,
    },
    KeePass {
        file_path: String,
        master_password: String,
    },
    Keeper {
        app_key: String,
        server: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "vault_connection")]
pub struct VaultConnection {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub provider: VaultProviderType,
    pub config_encrypted: Vec<u8>,
    pub nonce: Vec<u8>,
    pub enabled: bool,
    pub system_managed: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "vault_grant")]
pub struct VaultGrant {
    pub id: String,
    pub user_id: String,
    pub connection_id: String,
    pub vault_item_id: String,
    pub agent_id: String,
    pub query: String,
    pub env_var_prefix: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultSecret {
    pub id: String,
    pub name: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub notes: Option<String>,
    pub fields: HashMap<String, String>,
}

impl VaultSecret {
    pub fn field_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.username.is_some() {
            names.push("USERNAME".to_string());
        }
        if self.password.is_some() {
            names.push("PASSWORD".to_string());
        }
        for key in self.fields.keys() {
            names.push(key.to_uppercase().replace(' ', "_"));
        }
        names
    }

    pub fn to_env_vars(&self, prefix: &str) -> Vec<(String, String)> {
        let mut vars = Vec::new();
        if let Some(ref u) = self.username {
            vars.push((format!("{prefix}_USERNAME"), u.clone()));
        }
        if let Some(ref p) = self.password {
            vars.push((format!("{prefix}_PASSWORD"), p.clone()));
        }
        for (key, value) in &self.fields {
            let var_name = format!("{prefix}_{}", key.to_uppercase().replace(' ', "_"));
            vars.push((var_name, value.clone()));
        }
        vars
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultItem {
    pub id: String,
    pub name: String,
    pub username: Option<String>,
}

// --- Request/Response types ---

#[derive(Debug, Deserialize)]
pub struct CreateVaultConnectionRequest {
    pub name: String,
    pub provider: VaultProviderType,
    pub config: VaultConnectionConfig,
}

#[derive(Debug, Serialize)]
pub struct VaultConnectionResponse {
    pub id: String,
    pub name: String,
    pub provider: VaultProviderType,
    pub enabled: bool,
    pub system_managed: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<VaultConnection> for VaultConnectionResponse {
    fn from(c: VaultConnection) -> Self {
        Self {
            id: c.id,
            name: c.name,
            provider: c.provider,
            enabled: c.enabled,
            system_managed: c.system_managed,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ToggleVaultConnectionRequest {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct ApproveVaultRequest {
    pub chat_id: String,
    pub connection_id: String,
    pub vault_item_id: String,
    pub grant_duration: GrantDuration,
    pub env_var_prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DenyVaultRequest {
    pub chat_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantDuration {
    Once,
    Hours(u64),
    Days(u64),
    Permanent,
}

#[derive(Debug, Serialize)]
pub struct VaultGrantResponse {
    pub id: String,
    pub connection_id: String,
    pub vault_item_id: String,
    pub agent_id: String,
    pub query: String,
    pub env_var_prefix: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<VaultGrant> for VaultGrantResponse {
    fn from(g: VaultGrant) -> Self {
        Self {
            id: g.id,
            connection_id: g.connection_id,
            vault_item_id: g.vault_item_id,
            agent_id: g.agent_id,
            query: g.query,
            env_var_prefix: g.env_var_prefix,
            expires_at: g.expires_at,
            created_at: g.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_secret_to_env_vars() {
        let secret = VaultSecret {
            id: "1".into(),
            name: "GitHub".into(),
            username: Some("octocat".into()),
            password: Some("secret123".into()),
            notes: None,
            fields: {
                let mut m = HashMap::new();
                m.insert("api_key".into(), "ghp_abc123".into());
                m
            },
        };

        let vars = secret.to_env_vars("GITHUB");
        assert!(vars.contains(&("GITHUB_USERNAME".to_string(), "octocat".to_string())));
        assert!(vars.contains(&("GITHUB_PASSWORD".to_string(), "secret123".to_string())));
        assert!(vars.contains(&("GITHUB_API_KEY".to_string(), "ghp_abc123".to_string())));
    }

    #[test]
    fn vault_secret_field_names() {
        let secret = VaultSecret {
            id: "1".into(),
            name: "Test".into(),
            username: Some("user".into()),
            password: None,
            notes: None,
            fields: {
                let mut m = HashMap::new();
                m.insert("token key".into(), "val".into());
                m
            },
        };

        let names = secret.field_names();
        assert!(names.contains(&"USERNAME".to_string()));
        assert!(names.contains(&"TOKEN_KEY".to_string()));
        assert!(!names.contains(&"PASSWORD".to_string()));
    }

    #[test]
    fn vault_provider_type_display() {
        assert_eq!(VaultProviderType::Local.to_string(), "local");
        assert_eq!(VaultProviderType::OnePassword.to_string(), "one_password");
        assert_eq!(VaultProviderType::Hashicorp.to_string(), "hashicorp");
    }

    #[test]
    fn vault_connection_config_serde_round_trip() {
        let config = VaultConnectionConfig::Hashicorp {
            address: "http://localhost:8200".into(),
            token: "hvs.test".into(),
            mount_path: Some("secret".into()),
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: VaultConnectionConfig = serde_json::from_str(&json).unwrap();
        match deserialized {
            VaultConnectionConfig::Hashicorp { address, token, mount_path } => {
                assert_eq!(address, "http://localhost:8200");
                assert_eq!(token, "hvs.test");
                assert_eq!(mount_path.as_deref(), Some("secret"));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn vault_connection_response_redacts_config() {
        let conn = VaultConnection {
            id: "1".into(),
            user_id: "u".into(),
            name: "test".into(),
            provider: VaultProviderType::Hashicorp,
            config_encrypted: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
            enabled: true,
            system_managed: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let resp: VaultConnectionResponse = conn.into();
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("config_encrypted").is_none());
        assert!(json.get("nonce").is_none());
    }
}
