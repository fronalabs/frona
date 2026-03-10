use std::path::PathBuf;
use std::sync::Arc;

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;

use crate::core::config::VaultConfig;
use crate::credential::key_rotation::derive_key;
use crate::core::error::AppError;

use super::models::*;
use super::provider::{VaultProvider, create_local_provider, create_vault_provider};
use super::repository::{CredentialRepository, VaultAccessLogRepository, VaultConnectionRepository, VaultGrantRepository};

#[derive(Clone)]
pub struct VaultService {
    connection_repo: Arc<dyn VaultConnectionRepository>,
    grant_repo: Arc<dyn VaultGrantRepository>,
    credential_repo: Arc<dyn CredentialRepository>,
    access_log_repo: Arc<dyn VaultAccessLogRepository>,
    encryption_key: [u8; 32],
    vault_config: VaultConfig,
    data_dir: PathBuf,
    files_path: PathBuf,
}

impl VaultService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connection_repo: Arc<dyn VaultConnectionRepository>,
        grant_repo: Arc<dyn VaultGrantRepository>,
        credential_repo: Arc<dyn CredentialRepository>,
        access_log_repo: Arc<dyn VaultAccessLogRepository>,
        encryption_secret: &str,
        vault_config: VaultConfig,
        data_dir: PathBuf,
        files_path: PathBuf,
    ) -> Self {
        let encryption_key = derive_key(encryption_secret);

        Self {
            connection_repo,
            grant_repo,
            credential_repo,
            access_log_repo,
            encryption_key,
            vault_config,
            data_dir,
            files_path,
        }
    }

    // --- Connection CRUD ---

    pub async fn create_connection(
        &self,
        user_id: &str,
        req: CreateVaultConnectionRequest,
    ) -> Result<VaultConnectionResponse, AppError> {
        let (encrypted, nonce) = self.encrypt_config(&req.config)?;
        let now = Utc::now();
        let connection = VaultConnection {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: req.name,
            provider: req.provider,
            config_encrypted: encrypted,
            nonce,
            enabled: true,
            system_managed: false,
            created_at: now,
            updated_at: now,
        };
        let connection = self.connection_repo.create(&connection).await?;
        Ok(connection.into())
    }

    pub async fn list_connections(
        &self,
        user_id: &str,
    ) -> Result<Vec<VaultConnectionResponse>, AppError> {
        let connections = self
            .connection_repo
            .find_all_for_user(user_id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect();

        Ok(connections)
    }

    pub async fn delete_connection(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<(), AppError> {
        let connection = self
            .connection_repo
            .find_by_id(connection_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Vault connection not found".into()))?;
        if connection.system_managed {
            return Err(AppError::Validation("Cannot delete system-managed connections".into()));
        }
        if connection.user_id != user_id {
            return Err(AppError::Forbidden("Not your vault connection".into()));
        }
        self.grant_repo.delete_by_connection_id(connection_id).await?;
        self.connection_repo.delete(connection_id).await
    }

    pub async fn toggle_connection(
        &self,
        user_id: &str,
        connection_id: &str,
        enabled: bool,
    ) -> Result<VaultConnectionResponse, AppError> {
        let mut connection = self
            .connection_repo
            .find_by_id(connection_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Vault connection not found".into()))?;
        if connection.user_id != user_id {
            return Err(AppError::Forbidden("Not your vault connection".into()));
        }
        connection.enabled = enabled;
        connection.updated_at = Utc::now();
        let connection = self.connection_repo.update(&connection).await?;
        Ok(connection.into())
    }

    pub async fn test_connection(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<(), AppError> {
        let provider = self.get_provider(user_id, connection_id).await?;
        provider.test_connection().await
    }

    pub async fn search_items(
        &self,
        user_id: &str,
        connection_id: &str,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<VaultItem>, AppError> {
        let provider = self.get_provider(user_id, connection_id).await?;
        provider.search(query, max_results).await
    }

    // --- Grants ---

    pub async fn find_matching_grant(
        &self,
        user_id: &str,
        agent_id: &str,
        query: &str,
        env_var_prefix: Option<&str>,
    ) -> Result<Option<VaultGrant>, AppError> {
        let grant = self
            .grant_repo
            .find_matching_grant(user_id, agent_id, query, env_var_prefix)
            .await?;

        if let Some(ref g) = grant {
            if let Some(expires_at) = g.expires_at
                && expires_at < Utc::now()
            {
                self.grant_repo.delete(&g.id).await?;
                return Ok(None);
            }

            let conn = self.connection_repo.find_by_id(&g.connection_id).await?;
            if !conn.is_some_and(|c| c.enabled) {
                return Ok(None);
            }
        }

        Ok(grant)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_grant(
        &self,
        user_id: &str,
        agent_id: &str,
        connection_id: &str,
        vault_item_id: &str,
        query: &str,
        env_var_prefix: Option<&str>,
        duration: &GrantDuration,
    ) -> Result<VaultGrant, AppError> {
        let expires_at = match duration {
            GrantDuration::Once => {
                return Err(AppError::Validation(
                    "Once duration does not create grants; use log_access() instead".into(),
                ));
            }
            GrantDuration::Hours(h) => Some(Utc::now() + chrono::Duration::hours(*h as i64)),
            GrantDuration::Days(d) => Some(Utc::now() + chrono::Duration::days(*d as i64)),
            GrantDuration::Permanent => None,
        };

        let grant = VaultGrant {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            connection_id: connection_id.to_string(),
            vault_item_id: vault_item_id.to_string(),
            agent_id: agent_id.to_string(),
            query: query.to_string(),
            env_var_prefix: env_var_prefix.map(String::from),
            expires_at,
            created_at: Utc::now(),
        };
        self.grant_repo.create(&grant).await
    }

    // --- Access logging ---

    #[allow(clippy::too_many_arguments)]
    pub async fn log_access(
        &self,
        user_id: &str,
        agent_id: &str,
        chat_id: &str,
        connection_id: &str,
        vault_item_id: &str,
        env_var_prefix: Option<&str>,
        query: &str,
        reason: &str,
    ) -> Result<VaultAccessLog, AppError> {
        let log = VaultAccessLog {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            agent_id: agent_id.to_string(),
            chat_id: chat_id.to_string(),
            connection_id: connection_id.to_string(),
            vault_item_id: vault_item_id.to_string(),
            env_var_prefix: env_var_prefix.map(String::from),
            query: query.to_string(),
            reason: reason.to_string(),
            created_at: Utc::now(),
        };
        self.access_log_repo.create(&log).await
    }

    pub async fn find_existing_access(
        &self,
        chat_id: &str,
        query: &str,
        env_var_prefix: Option<&str>,
    ) -> Result<Option<VaultAccessLog>, AppError> {
        self.access_log_repo
            .find_by_chat_and_query(chat_id, query, env_var_prefix)
            .await
    }

    pub async fn hydrate_chat_env_vars(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> Result<Vec<(String, String)>, AppError> {
        let logs = self.access_log_repo.find_by_chat_id(chat_id).await?;
        let mut env_vars = Vec::new();

        for log in logs {
            let Some(ref prefix) = log.env_var_prefix else {
                continue;
            };
            match self
                .get_secret(user_id, &log.connection_id, &log.vault_item_id)
                .await
            {
                Ok(secret) => env_vars.extend(secret.to_env_vars(prefix)),
                Err(e) => {
                    tracing::warn!(
                        vault_item_id = %log.vault_item_id,
                        error = %e,
                        "Failed to fetch secret for access log entry"
                    );
                }
            }
        }

        Ok(env_vars)
    }

    pub async fn list_grants(
        &self,
        user_id: &str,
    ) -> Result<Vec<VaultGrantResponse>, AppError> {
        let grants = self.grant_repo.find_by_user_id(user_id).await?;
        Ok(grants.into_iter().map(Into::into).collect())
    }

    pub async fn revoke_grant(
        &self,
        user_id: &str,
        grant_id: &str,
    ) -> Result<(), AppError> {
        let grant = self
            .grant_repo
            .find_by_id(grant_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Grant not found".into()))?;
        if grant.user_id != user_id {
            return Err(AppError::Forbidden("Not your grant".into()));
        }
        self.grant_repo.delete(grant_id).await
    }

    // --- Secret fetching ---

    pub async fn get_secret(
        &self,
        user_id: &str,
        connection_id: &str,
        item_id: &str,
    ) -> Result<VaultSecret, AppError> {
        let provider = self.get_provider(user_id, connection_id).await?;
        provider.get_secret(item_id).await
    }

    // --- Search across all connections ---

    pub async fn search_all(
        &self,
        user_id: &str,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<(String, VaultItem)>, AppError> {
        let connections = self.list_connections(user_id).await?;
        let mut all_results = Vec::new();

        for conn in connections {
            if !conn.enabled {
                continue;
            }
            match self.search_items(user_id, &conn.id, query, max_results).await {
                Ok(items) => {
                    for item in items {
                        all_results.push((conn.id.clone(), item));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        connection_id = %conn.id,
                        error = %e,
                        "Failed to search vault connection"
                    );
                }
            }
        }

        all_results.truncate(max_results);
        Ok(all_results)
    }

    // --- Startup sync ---

    pub async fn sync_config_connections(&self) -> Result<(), AppError> {
        let system_user = "system";
        let now = Utc::now();

        let desired = self.config_connection_entries();
        let existing = self.connection_repo.find_system_managed().await?;

        let existing_ids: std::collections::HashSet<&str> =
            existing.iter().map(|c| c.id.as_str()).collect();
        let desired_ids: std::collections::HashSet<&str> =
            desired.iter().map(|(id, _, _, _)| id.as_str()).collect();

        for (id, provider, name, config) in &desired {
            let (encrypted, nonce) = self.encrypt_config(config)?;
            let connection = VaultConnection {
                id: id.clone(),
                user_id: system_user.to_string(),
                name: name.clone(),
                provider: *provider,
                config_encrypted: encrypted,
                nonce,
                enabled: true,
                system_managed: true,
                created_at: now,
                updated_at: now,
            };
            if existing_ids.contains(id.as_str()) {
                self.connection_repo.update(&connection).await?;
            } else {
                self.connection_repo.create(&connection).await?;
            }
        }

        for conn in &existing {
            if !desired_ids.contains(conn.id.as_str()) && conn.id != "local" && conn.enabled {
                tracing::info!(
                    connection_id = %conn.id,
                    provider = %conn.provider,
                    "Disabling config vault connection (config removed)"
                );
                let mut disabled = conn.clone();
                disabled.enabled = false;
                disabled.updated_at = now;
                self.connection_repo.update(&disabled).await?;
            }
        }

        let local_id = "local";
        if !existing_ids.contains(local_id) {
            let local = VaultConnection {
                id: local_id.to_string(),
                user_id: system_user.to_string(),
                name: "Local Credentials".to_string(),
                provider: VaultProviderType::Local,
                config_encrypted: Vec::new(),
                nonce: Vec::new(),
                enabled: true,
                system_managed: true,
                created_at: now,
                updated_at: now,
            };
            self.connection_repo.create(&local).await?;
        }

        Ok(())
    }

    fn config_connection_entries(&self) -> Vec<(String, VaultProviderType, String, VaultConnectionConfig)> {
        let mut entries = Vec::new();

        if let Some(token) = &self.vault_config.onepassword_service_account_token {
            entries.push((
                "onepassword".to_string(),
                VaultProviderType::OnePassword,
                "1Password (config)".to_string(),
                VaultConnectionConfig::OnePassword {
                    service_account_token: token.clone(),
                    default_vault_id: self.vault_config.onepassword_vault_id.clone(),
                },
            ));
        }

        if let (Some(client_id), Some(client_secret), Some(master_password)) = (
            &self.vault_config.bitwarden_client_id,
            &self.vault_config.bitwarden_client_secret,
            &self.vault_config.bitwarden_master_password,
        ) {
            entries.push((
                "bitwarden".to_string(),
                VaultProviderType::Bitwarden,
                "Bitwarden (config)".to_string(),
                VaultConnectionConfig::Bitwarden {
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    master_password: master_password.clone(),
                    server_url: self.vault_config.bitwarden_server_url.clone(),
                },
            ));
        }

        if let (Some(address), Some(token)) = (
            &self.vault_config.hashicorp_address,
            &self.vault_config.hashicorp_token,
        ) {
            entries.push((
                "hashicorp".to_string(),
                VaultProviderType::Hashicorp,
                "HashiCorp Vault (config)".to_string(),
                VaultConnectionConfig::Hashicorp {
                    address: address.clone(),
                    token: token.clone(),
                    mount_path: self.vault_config.hashicorp_mount.clone(),
                },
            ));
        }

        if let (Some(path), Some(password)) = (
            &self.vault_config.keepass_path,
            &self.vault_config.keepass_password,
        ) {
            entries.push((
                "keepass".to_string(),
                VaultProviderType::KeePass,
                "KeePass (config)".to_string(),
                VaultConnectionConfig::KeePass {
                    file_path: path.clone(),
                    master_password: password.clone(),
                },
            ));
        }

        if let Some(app_key) = &self.vault_config.keeper_app_key {
            entries.push((
                "keeper".to_string(),
                VaultProviderType::Keeper,
                "Keeper (config)".to_string(),
                VaultConnectionConfig::Keeper {
                    app_key: app_key.clone(),
                    server: None,
                },
            ));
        }

        entries
    }

    // --- Credential CRUD (merged from CredentialService) ---

    pub async fn create_credential(
        &self,
        user_id: &str,
        req: CreateLocalItemRequest,
    ) -> Result<CredentialResponse, AppError> {
        let now = Utc::now();

        let (name, provider, data) = match req {
            CreateLocalItemRequest::BrowserProfile { name } => {
                (name, "browser".to_string(), CredentialData::BrowserProfile)
            }
            CreateLocalItemRequest::UsernamePassword { name, username, password } => {
                (
                    name,
                    "local".to_string(),
                    CredentialData::UsernamePassword {
                        username,
                        password_encrypted: encrypt_password(&password, &self.encryption_key)?,
                    },
                )
            }
            CreateLocalItemRequest::ApiKey { name, api_key } => {
                (
                    name,
                    "local".to_string(),
                    CredentialData::ApiKey {
                        key_encrypted: encrypt_password(&api_key, &self.encryption_key)?,
                    },
                )
            }
        };

        let credential = Credential {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name,
            provider,
            data,
            created_at: now,
            updated_at: now,
        };

        let credential = self.credential_repo.create(&credential).await?;
        Ok(credential.into())
    }

    pub async fn list_credentials(
        &self,
        user_id: &str,
    ) -> Result<Vec<CredentialResponse>, AppError> {
        let credentials = self.credential_repo.find_by_user_id(user_id).await?;
        Ok(credentials.into_iter().map(Into::into).collect())
    }

    pub async fn find_credential_by_id(&self, id: &str) -> Result<Option<Credential>, AppError> {
        self.credential_repo.find_by_id(id).await
    }

    pub async fn find_credential_by_user_and_provider(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<Option<Credential>, AppError> {
        self.credential_repo.find_by_user_and_provider(user_id, provider).await
    }

    pub async fn update_credential(
        &self,
        user_id: &str,
        credential_id: &str,
        req: UpdateLocalItemRequest,
    ) -> Result<CredentialResponse, AppError> {
        let existing = self
            .credential_repo
            .find_by_id(credential_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Credential not found".into()))?;

        if existing.user_id != user_id {
            return Err(AppError::Forbidden("Not your credential".into()));
        }

        let now = Utc::now();

        let (name, provider, data) = match req {
            UpdateLocalItemRequest::BrowserProfile { name } => {
                (name, "browser".to_string(), CredentialData::BrowserProfile)
            }
            UpdateLocalItemRequest::UsernamePassword { name, username, password } => {
                let password_encrypted = if let Some(pw) = password {
                    encrypt_password(&pw, &self.encryption_key)?
                } else {
                    match &existing.data {
                        CredentialData::UsernamePassword { password_encrypted, .. } => password_encrypted.clone(),
                        _ => return Err(AppError::Validation("Credential type mismatch".into())),
                    }
                };
                (
                    name,
                    "local".to_string(),
                    CredentialData::UsernamePassword {
                        username,
                        password_encrypted,
                    },
                )
            }
            UpdateLocalItemRequest::ApiKey { name, api_key } => {
                let key_encrypted = if let Some(key) = api_key {
                    encrypt_password(&key, &self.encryption_key)?
                } else {
                    match &existing.data {
                        CredentialData::ApiKey { key_encrypted, .. } => key_encrypted.clone(),
                        _ => return Err(AppError::Validation("Credential type mismatch".into())),
                    }
                };
                (
                    name,
                    "local".to_string(),
                    CredentialData::ApiKey { key_encrypted },
                )
            }
        };

        let credential = Credential {
            id: existing.id,
            user_id: existing.user_id,
            name,
            provider,
            data,
            created_at: existing.created_at,
            updated_at: now,
        };

        let credential = self.credential_repo.update(&credential).await?;
        Ok(credential.into())
    }

    pub async fn delete_credential(
        &self,
        user_id: &str,
        credential_id: &str,
    ) -> Result<(), AppError> {
        let credential = self
            .credential_repo
            .find_by_id(credential_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Credential not found".into()))?;

        if credential.user_id != user_id {
            return Err(AppError::Forbidden("Not your credential".into()));
        }

        self.credential_repo.delete(credential_id).await
    }

    // --- Internal helpers ---

    async fn get_provider(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<Box<dyn VaultProvider>, AppError> {
        let connection = self
            .connection_repo
            .find_by_id(connection_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Vault connection not found".into()))?;

        if !connection.system_managed && connection.user_id != user_id {
            return Err(AppError::Forbidden("Not your vault connection".into()));
        }

        if !connection.enabled {
            return Err(AppError::Validation("Vault connection is disabled".into()));
        }

        if connection.provider == VaultProviderType::Local {
            return Ok(create_local_provider(
                self.credential_repo.clone(),
                self.encryption_key,
                user_id.to_string(),
            ));
        }

        let config = self.decrypt_config(&connection)?;

        let home_dir = if connection.system_managed {
            self.data_dir.join("system").join("vault").join(connection.id)
        } else {
            self.files_path.join(&connection.user_id)
        };

        create_vault_provider(connection.provider, config, home_dir)
    }

    fn encrypt_config(
        &self,
        config: &VaultConnectionConfig,
    ) -> Result<(Vec<u8>, Vec<u8>), AppError> {
        let json = serde_json::to_vec(config)
            .map_err(|e| AppError::Internal(format!("Config serialization failed: {e}")))?;

        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

        let nonce_bytes: [u8; 12] = rand::random();
        let nonce = Nonce::from_slice(&nonce_bytes);

        let encrypted = cipher
            .encrypt(nonce, json.as_ref())
            .map_err(|e| AppError::Internal(format!("Encryption failed: {e}")))?;

        Ok((encrypted, nonce_bytes.to_vec()))
    }

    fn decrypt_config(
        &self,
        connection: &VaultConnection,
    ) -> Result<VaultConnectionConfig, AppError> {
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

        let nonce = Nonce::from_slice(&connection.nonce);
        let decrypted = cipher
            .decrypt(nonce, connection.config_encrypted.as_ref())
            .map_err(|e| AppError::Internal(format!("Decryption failed: {e}")))?;

        serde_json::from_slice(&decrypted)
            .map_err(|e| AppError::Internal(format!("Config deserialization failed: {e}")))
    }
}

pub fn encrypt_password(password: &str, key: &[u8; 32]) -> Result<String, AppError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let encrypted = cipher
        .encrypt(nonce, password.as_bytes())
        .map_err(|e| AppError::Internal(format!("Encryption failed: {e}")))?;

    let mut combined = nonce_bytes.to_vec();
    combined.extend_from_slice(&encrypted);
    Ok(URL_SAFE_NO_PAD.encode(&combined))
}

pub fn decrypt_password(encrypted_b64: &str, key: &[u8; 32]) -> Result<String, AppError> {
    let combined = URL_SAFE_NO_PAD
        .decode(encrypted_b64)
        .map_err(|e| AppError::Internal(format!("Base64 decode failed: {e}")))?;

    if combined.len() < 12 {
        return Err(AppError::Internal("Encrypted data too short".into()));
    }

    let (nonce_bytes, encrypted_data) = combined.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

    let decrypted = cipher
        .decrypt(nonce, encrypted_data)
        .map_err(|e| AppError::Internal(format!("Decryption failed: {e}")))?;

    String::from_utf8(decrypted)
        .map_err(|e| AppError::Internal(format!("UTF8 decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        derive_key("test-secret")
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = test_key();
        let password = "my-secret-password";

        let encrypted = encrypt_password(password, &key).unwrap();
        assert_ne!(encrypted, password);

        let decrypted = decrypt_password(&encrypted, &key).unwrap();
        assert_eq!(decrypted, password);
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let key = test_key();
        let encrypted = encrypt_password("secret", &key).unwrap();

        let mut wrong_key = [0u8; 32];
        wrong_key[0] = 1;
        let result = decrypt_password(&encrypted, &wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn decrypt_too_short_data_fails() {
        let key = test_key();
        let short = URL_SAFE_NO_PAD.encode(b"short");
        let result = decrypt_password(&short, &key);
        assert!(result.is_err());
    }

    #[test]
    fn each_encryption_produces_different_ciphertext() {
        let key = test_key();
        let e1 = encrypt_password("same", &key).unwrap();
        let e2 = encrypt_password("same", &key).unwrap();
        assert_ne!(e1, e2);
    }
}
