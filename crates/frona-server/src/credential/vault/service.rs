use std::path::PathBuf;
use std::sync::Arc;

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;

use crate::core::Principal;
use crate::core::config::VaultConfig;
use crate::core::error::AppError;
use crate::core::principal::PrincipalKind;
use crate::credential::key_rotation::derive_key;

use super::models::*;
use super::provider::{VaultProvider, create_local_provider, create_vault_provider};
use super::repository::{
    CredentialRepository, PrincipalCredentialBindingRepository, VaultAccessLogRepository,
    VaultConnectionRepository, VaultGrantRepository,
};

#[derive(Clone)]
pub struct VaultService {
    connection_repo: Arc<dyn VaultConnectionRepository>,
    grant_repo: Arc<dyn VaultGrantRepository>,
    credential_repo: Arc<dyn CredentialRepository>,
    access_log_repo: Arc<dyn VaultAccessLogRepository>,
    binding_repo: Arc<dyn PrincipalCredentialBindingRepository>,
    encryption_key: [u8; 32],
    vault_config: VaultConfig,
    data_dir: PathBuf,
    storage: crate::storage::service::StorageService,
    user_service: crate::auth::UserService,
    managed_vault: crate::credential::managed::ManagedVault,
    managed_resolver: Arc<crate::credential::managed::resolver::ManagedResolver>,
    login_service: crate::credential::managed::login::ManagedLoginService,
}

fn ensure_non_user_principal(principal: &Principal) -> Result<(), AppError> {
    if matches!(principal.kind, PrincipalKind::User) {
        return Err(AppError::Validation(
            "vault grants and bindings cannot target the User principal kind".into(),
        ));
    }
    Ok(())
}

impl VaultService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connection_repo: Arc<dyn VaultConnectionRepository>,
        grant_repo: Arc<dyn VaultGrantRepository>,
        credential_repo: Arc<dyn CredentialRepository>,
        access_log_repo: Arc<dyn VaultAccessLogRepository>,
        binding_repo: Arc<dyn PrincipalCredentialBindingRepository>,
        encryption_secret: &str,
        vault_config: VaultConfig,
        data_dir: PathBuf,
        storage: crate::storage::service::StorageService,
        user_service: crate::auth::UserService,
        managed_vault: crate::credential::managed::ManagedVault,
        managed_resolver: Arc<crate::credential::managed::resolver::ManagedResolver>,
        login_service: crate::credential::managed::login::ManagedLoginService,
    ) -> Self {
        let encryption_key = derive_key(encryption_secret);

        Self {
            connection_repo,
            grant_repo,
            credential_repo,
            access_log_repo,
            binding_repo,
            encryption_key,
            vault_config,
            data_dir,
            storage,
            user_service,
            managed_vault,
            managed_resolver,
            login_service,
        }
    }

    pub fn login_service(&self) -> &crate::credential::managed::login::ManagedLoginService {
        &self.login_service
    }

    pub async fn advance_login(
        &self,
        user_id: &str,
        attempt: uuid::Uuid,
        completion: Option<&str>,
    ) -> Result<crate::credential::managed::login::service::LoginAttempt, AppError> {
        self.login_service
            .advance_authorized(user_id, attempt, completion, |connection| async move {
                self.managed_login_vault(user_id, &connection)
                    .await
                    .map(|_| ())
            })
            .await
    }

    pub async fn create_connection(
        &self,
        user_id: &str,
        req: CreateVaultConnectionRequest,
    ) -> Result<VaultConnectionResponse, AppError> {
        if (req.provider == VaultProviderType::Managed)
            != matches!(req.config, VaultConnectionConfig::Managed {})
        {
            return Err(AppError::Validation(
                "managed vault config requires managed provider".into(),
            ));
        }
        let (encrypted, nonce) = self.encrypt_config(&req.config)?;
        let now = Utc::now();
        let connection = VaultConnection {
            id: crate::core::repository::new_id(),
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

    pub(crate) async fn get_connection(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<VaultConnection, AppError> {
        let connection = self
            .connection_repo
            .find_by_id(connection_id)
            .await?
            .ok_or_else(|| AppError::NotFound("vault connection".into()))?;
        if !connection.enabled || (!connection.system_managed && connection.user_id != user_id) {
            return Err(AppError::Forbidden("vault connection unavailable".into()));
        }
        Ok(connection)
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
            return Err(AppError::Validation(
                "Cannot delete system-managed connections".into(),
            ));
        }
        if connection.user_id != user_id {
            return Err(AppError::Forbidden("Not your vault connection".into()));
        }
        if connection.provider == VaultProviderType::Managed {
            return self
                .managed_vault
                .for_connection(connection.id)
                .delete_connection(user_id)
                .await;
        }
        self.grant_repo
            .delete_by_connection_id(connection_id)
            .await?;
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

    pub async fn find_matching_grant(
        &self,
        user_id: &str,
        principal: &Principal,
        query: &str,
    ) -> Result<Option<VaultGrant>, AppError> {
        ensure_non_user_principal(principal)?;
        let grant = self
            .grant_repo
            .find_matching_grant(user_id, principal, query)
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

    pub async fn create_grant(
        &self,
        user_id: &str,
        principal: Principal,
        connection_id: &str,
        vault_item_id: &str,
        query: &str,
        duration: &GrantDuration,
    ) -> Result<VaultGrant, AppError> {
        ensure_non_user_principal(&principal)?;
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
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            connection_id: connection_id.to_string(),
            vault_item_id: vault_item_id.to_string(),
            principal,
            query: query.to_string(),
            expires_at,
            created_at: Utc::now(),
        };
        self.grant_repo.create(&grant).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn log_access(
        &self,
        user_id: &str,
        principal: Principal,
        chat_id: &str,
        connection_id: &str,
        vault_item_id: &str,
        env_var_prefix: Option<&str>,
        query: &str,
        reason: &str,
    ) -> Result<VaultAccessLog, AppError> {
        ensure_non_user_principal(&principal)?;
        let log = VaultAccessLog {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            principal,
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

    /// Resolve under the current binding and grant, including a recheck after
    /// remote integration work. Chat-only approval is represented by its binding.
    pub async fn resolve_binding(
        &self,
        user_id: &str,
        principal: &Principal,
        binding: &PrincipalCredentialBinding,
        chat_id: Option<&str>,
    ) -> Result<VaultSecret, AppError> {
        self.authorize_binding(user_id, principal, binding, chat_id)
            .await?;
        let secret = self
            .get_secret(user_id, &binding.connection_id, &binding.vault_item_id)
            .await?;
        self.authorize_binding(user_id, principal, binding, chat_id)
            .await?;
        project_target(&secret, &binding.target)?;
        Ok(secret)
    }

    async fn authorize_binding(
        &self,
        user_id: &str,
        principal: &Principal,
        binding: &PrincipalCredentialBinding,
        chat_id: Option<&str>,
    ) -> Result<(), AppError> {
        ensure_non_user_principal(principal)?;
        let denied = || {
            AppError::Forbidden(format!(
                "vault binding {} is no longer authorized",
                binding.id
            ))
        };
        let current = self
            .binding_repo
            .find_by_id(&binding.id)
            .await?
            .ok_or_else(denied)?;
        if current.user_id != user_id
            || &current.principal != principal
            || current.connection_id != binding.connection_id
            || current.vault_item_id != binding.vault_item_id
            || current.target != binding.target
            || current.scope != binding.scope
            || current
                .expires_at
                .is_some_and(|expiry| expiry <= Utc::now())
        {
            return Err(denied());
        }
        match &current.scope {
            BindingScope::Chat { chat_id: expected } if chat_id == Some(expected.as_str()) => {}
            BindingScope::Durable
                if self
                    .has_grant_for_item(
                        user_id,
                        principal,
                        &current.connection_id,
                        &current.vault_item_id,
                    )
                    .await? => {}
            _ => return Err(denied()),
        }
        let connection = self
            .connection_repo
            .find_by_id(&current.connection_id)
            .await?
            .ok_or_else(denied)?;
        if !connection.enabled || (!connection.system_managed && connection.user_id != user_id) {
            return Err(denied());
        }
        Ok(())
    }

    /// Resolve a fresh environment for one consumer start. Never retain it on a
    /// chat or merge it with credential values from a previous start.
    pub async fn resolve_env(
        &self,
        user_id: &str,
        principal: &Principal,
        chat_id: Option<&str>,
    ) -> Result<Vec<(String, String)>, AppError> {
        let bindings = self.startup_bindings(user_id, principal, chat_id).await?;
        let mut env = Vec::new();
        for binding in &bindings {
            let secret = self
                .resolve_binding(user_id, principal, binding, chat_id)
                .await
                .map_err(|_| {
                    AppError::Validation(format!(
                        "required vault binding {} for item {} could not be resolved",
                        binding.id, binding.vault_item_id,
                    ))
                })?;
            env.extend(project_target(&secret, &binding.target)?);
        }
        // A slow later integration must not let a revoked earlier binding
        // deliver its already-resolved fields.
        for binding in &bindings {
            self.authorize_binding(user_id, principal, binding, chat_id)
                .await?;
        }
        let current = self.startup_bindings(user_id, principal, chat_id).await?;
        let ids = |items: &[PrincipalCredentialBinding]| {
            items
                .iter()
                .map(|b| b.id.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        if ids(&current) != ids(&bindings) {
            return Err(AppError::Conflict(
                "vault bindings changed during startup".into(),
            ));
        }
        Ok(env)
    }

    async fn startup_bindings(
        &self,
        user_id: &str,
        principal: &Principal,
        chat_id: Option<&str>,
    ) -> Result<Vec<PrincipalCredentialBinding>, AppError> {
        ensure_non_user_principal(principal)?;
        match chat_id {
            Some(chat_id) => {
                self.binding_repo
                    .find_for_chat(user_id, principal, chat_id)
                    .await
            }
            None => Ok(self
                .binding_repo
                .find_for_principal(user_id, principal)
                .await?
                .into_iter()
                .filter(|b| matches!(b.scope, BindingScope::Durable))
                .collect()),
        }
    }

    pub async fn has_grant_for_item(
        &self,
        user_id: &str,
        principal: &Principal,
        connection_id: &str,
        vault_item_id: &str,
    ) -> Result<bool, AppError> {
        ensure_non_user_principal(principal)?;
        let grants = self
            .grant_repo
            .find_by_principal(user_id, principal)
            .await?;
        Ok(grants.iter().any(|g| {
            g.connection_id == connection_id
                && g.vault_item_id == vault_item_id
                && g.expires_at.is_none_or(|expiry| expiry > Utc::now())
        }))
    }

    pub async fn delete_grants_for_principal(
        &self,
        user_id: &str,
        principal: &Principal,
    ) -> Result<(), AppError> {
        ensure_non_user_principal(principal)?;
        self.grant_repo
            .delete_by_principal(user_id, principal)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_binding(
        &self,
        user_id: &str,
        principal: Principal,
        query: &str,
        connection_id: &str,
        vault_item_id: &str,
        target: CredentialTarget,
        scope: BindingScope,
        expires_at: Option<chrono::DateTime<Utc>>,
    ) -> Result<PrincipalCredentialBinding, AppError> {
        ensure_non_user_principal(&principal)?;
        let binding = PrincipalCredentialBinding {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            principal,
            query: query.to_string(),
            connection_id: connection_id.to_string(),
            vault_item_id: vault_item_id.to_string(),
            target,
            scope,
            expires_at,
            created_at: Utc::now(),
        };
        self.binding_repo.create(&binding).await
    }

    pub async fn find_binding(
        &self,
        user_id: &str,
        principal: &Principal,
        query: &str,
        chat_id: Option<&str>,
    ) -> Result<Option<PrincipalCredentialBinding>, AppError> {
        ensure_non_user_principal(principal)?;
        self.binding_repo
            .find_for_lookup(user_id, principal, query, chat_id)
            .await
    }

    pub async fn list_bindings_for_principal(
        &self,
        user_id: &str,
        principal: &Principal,
    ) -> Result<Vec<PrincipalCredentialBinding>, AppError> {
        ensure_non_user_principal(principal)?;
        self.binding_repo
            .find_for_principal(user_id, principal)
            .await
    }

    pub async fn delete_bindings_for_principal(
        &self,
        user_id: &str,
        principal: &Principal,
    ) -> Result<(), AppError> {
        ensure_non_user_principal(principal)?;
        self.binding_repo
            .delete_by_principal(user_id, principal)
            .await
    }
}

pub fn project_target(
    secret: &VaultSecret,
    target: &CredentialTarget,
) -> Result<Vec<(String, String)>, AppError> {
    match target {
        CredentialTarget::Prefix { env_var_prefix } => Ok(secret.to_env_vars(env_var_prefix)),
        CredentialTarget::Single { env_var, field } => {
            let custom = |name: &str| {
                secret.fields.get(name).cloned().or_else(|| {
                    secret.fields.iter().find_map(|(key, value)| {
                        (key.to_uppercase().replace(' ', "_") == name.to_uppercase())
                            .then(|| value.clone())
                    })
                })
            };
            let value = match field {
                VaultField::Password => secret.password.clone().or_else(|| custom("PASSWORD")),
                VaultField::Username => secret.username.clone().or_else(|| custom("USERNAME")),
                VaultField::Custom { name } => custom(name),
            };
            value.map(|v| vec![(env_var.clone(), v)]).ok_or_else(|| {
                AppError::Validation(format!(
                    "required field is missing from vault item {}",
                    secret.id
                ))
            })
        }
    }
}

impl VaultService {
    pub async fn list_grants(&self, user_id: &str) -> Result<Vec<VaultGrantResponse>, AppError> {
        let grants = self.grant_repo.find_by_user_id(user_id).await?;
        let mut responses: Vec<VaultGrantResponse> = grants.into_iter().map(Into::into).collect();

        let mut seen = std::collections::HashSet::new();
        let mut all_bindings = Vec::new();
        for r in &responses {
            let key = format!("{:?}:{}", r.principal.kind, r.principal.id);
            if seen.insert(key)
                && let Ok(bindings) = self
                    .binding_repo
                    .find_for_principal(user_id, &r.principal)
                    .await
            {
                all_bindings.extend(bindings);
            }
        }

        for r in &mut responses {
            if let Some(binding) = all_bindings.iter().find(|b| {
                b.connection_id == r.connection_id
                    && b.vault_item_id == r.vault_item_id
                    && b.query == r.query
            }) {
                r.target = Some(binding.target.clone());
            }
        }

        Ok(responses)
    }

    pub async fn revoke_grant(&self, user_id: &str, grant_id: &str) -> Result<(), AppError> {
        let grant = self
            .grant_repo
            .find_by_id(grant_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Grant not found".into()))?;
        if grant.user_id != user_id {
            return Err(AppError::Forbidden("Not your grant".into()));
        }
        self.binding_repo
            .delete_for_item(
                user_id,
                &grant.principal,
                &grant.connection_id,
                &grant.vault_item_id,
            )
            .await?;
        self.grant_repo.delete(grant_id).await
    }

    pub async fn get_secret(
        &self,
        user_id: &str,
        connection_id: &str,
        item_id: &str,
    ) -> Result<VaultSecret, AppError> {
        let provider = self.get_provider(user_id, connection_id).await?;
        provider.get_secret(item_id).await
    }

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
            match self
                .search_items(user_id, &conn.id, query, max_results)
                .await
            {
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

    fn config_connection_entries(
        &self,
    ) -> Vec<(String, VaultProviderType, String, VaultConnectionConfig)> {
        let mut entries = Vec::new();
        entries.push((
            crate::credential::managed::GLOBAL_CONNECTION_ID.into(),
            VaultProviderType::Managed,
            "Managed credentials (shared)".into(),
            VaultConnectionConfig::Managed {},
        ));

        if let Some(token) = &self.vault_config.onepassword_service_account_token {
            entries.push((
                "onepassword".to_string(),
                VaultProviderType::OnePassword,
                "1Password".to_string(),
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
                "Bitwarden".to_string(),
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
                "HashiCorp Vault".to_string(),
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
                "KeePass".to_string(),
                VaultConnectionConfig::KeePass {
                    file_path: path.clone(),
                    master_password: password.clone(),
                },
            ));
        }

        entries
    }

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
            CreateLocalItemRequest::UsernamePassword {
                name,
                username,
                password,
            } => (
                name,
                "local".to_string(),
                CredentialData::UsernamePassword {
                    username,
                    password_encrypted: encrypt_password(&password, &self.encryption_key)?,
                },
            ),
            CreateLocalItemRequest::ApiKey { name, api_key } => (
                name,
                "local".to_string(),
                CredentialData::ApiKey {
                    key_encrypted: encrypt_password(&api_key, &self.encryption_key)?,
                },
            ),
        };

        let credential = Credential {
            id: crate::core::repository::new_id(),
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
        self.credential_repo
            .find_by_user_and_provider(user_id, provider)
            .await
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
            UpdateLocalItemRequest::UsernamePassword {
                name,
                username,
                password,
            } => {
                let password_encrypted = if let Some(pw) = password {
                    encrypt_password(&pw, &self.encryption_key)?
                } else {
                    match &existing.data {
                        CredentialData::UsernamePassword {
                            password_encrypted, ..
                        } => password_encrypted.clone(),
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

    /// Current management authorization for a configured managed vault.
    pub(crate) async fn managed_login_vault(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<crate::credential::managed::ManagedVault, AppError> {
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .filter(|user| user.deactivated_at.is_none())
            .ok_or_else(|| AppError::Forbidden("active user required".into()))?;
        let connection = self
            .connection_repo
            .find_by_id(connection_id)
            .await?
            .ok_or_else(|| AppError::NotFound("vault connection".into()))?;
        if !connection.enabled || (!connection.system_managed && connection.user_id != user_id) {
            return Err(AppError::Forbidden(
                "vault connection is unavailable".into(),
            ));
        }
        if connection.system_managed
            && !user
                .groups
                .iter()
                .any(|group| group == crate::auth::models::ADMINS_GROUP)
        {
            return Err(AppError::Forbidden(
                "Administrator privileges required".into(),
            ));
        }
        if connection.provider != VaultProviderType::Managed {
            return Err(AppError::Validation(
                "login requires a managed vault".into(),
            ));
        }
        if !matches!(
            self.decrypt_config(&connection)?,
            VaultConnectionConfig::Managed {}
        ) {
            return Err(AppError::Validation(
                "invalid managed vault connection".into(),
            ));
        }
        let vault = self.managed_vault.for_connection(connection.id);
        vault.ensure_available().await?;
        Ok(vault)
    }

    pub(crate) async fn delete_managed_item(
        &self,
        user_id: &str,
        connection_id: &str,
        item_id: uuid::Uuid,
    ) -> Result<(), AppError> {
        let vault = self.managed_login_vault(user_id, connection_id).await?;
        let current = vault
            .status_by_id(item_id)
            .await?
            .ok_or_else(|| AppError::NotFound("managed credential".into()))?;
        vault.delete_by_id(item_id, current.version).await
    }

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
        if connection.provider == VaultProviderType::Managed {
            if !matches!(config, VaultConnectionConfig::Managed {}) {
                return Err(AppError::Validation(
                    "invalid managed vault connection".into(),
                ));
            }
            return Ok(Box::new(super::providers::managed::ManagedVaultProvider {
                vault: self.managed_vault.for_connection(connection.id.clone()),
                resolver: self.managed_resolver.clone(),
                connection,
                connections: self.connection_repo.clone(),
                user_id: user_id.into(),
            }));
        }

        let home_dir = if connection.system_managed {
            self.data_dir
                .join("system")
                .join("vault")
                .join(connection.id)
        } else {
            let owner = self
                .user_service
                .find_by_id(&connection.user_id)
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!("vault owner user {} not found", connection.user_id))
                })?;
            self.storage.user_vault_path(&owner.handle)
        };

        create_vault_provider(connection.provider, config, home_dir)
    }

    fn encrypt_config(
        &self,
        config: &VaultConnectionConfig,
    ) -> Result<(Vec<u8>, Vec<u8>), AppError> {
        if matches!(config, VaultConnectionConfig::Managed {}) {
            return Ok((Vec::new(), Vec::new()));
        }
        let json = serde_json::to_vec(config)
            .map_err(|e| AppError::Internal(format!("Config serialization failed: {e}")))?;

        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

        let nonce_bytes: [u8; 12] = rand::random();
        let nonce = Nonce::from(nonce_bytes);

        let encrypted = cipher
            .encrypt(&nonce, json.as_ref())
            .map_err(|e| AppError::Internal(format!("Encryption failed: {e}")))?;

        Ok((encrypted, nonce_bytes.to_vec()))
    }

    fn decrypt_config(
        &self,
        connection: &VaultConnection,
    ) -> Result<VaultConnectionConfig, AppError> {
        if connection.provider == VaultProviderType::Managed {
            return Ok(VaultConnectionConfig::Managed {});
        }
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

        let nonce_arr: [u8; 12] = connection
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| AppError::Internal("Invalid nonce length".into()))?;
        let nonce = Nonce::from(nonce_arr);
        let decrypted = cipher
            .decrypt(&nonce, connection.config_encrypted.as_ref())
            .map_err(|e| AppError::Internal(format!("Decryption failed: {e}")))?;

        serde_json::from_slice(&decrypted)
            .map_err(|e| AppError::Internal(format!("Config deserialization failed: {e}")))
    }
}

pub fn encrypt_password(password: &str, key: &[u8; 32]) -> Result<String, AppError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from(nonce_bytes);

    let encrypted = cipher
        .encrypt(&nonce, password.as_bytes())
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
    let nonce_arr: [u8; 12] = nonce_bytes
        .try_into()
        .map_err(|_| AppError::Internal("Invalid nonce length".into()))?;
    let nonce = Nonce::from(nonce_arr);

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| AppError::Internal(format!("AES init failed: {e}")))?;

    let decrypted = cipher
        .decrypt(&nonce, encrypted_data)
        .map_err(|e| AppError::Internal(format!("Decryption failed: {e}")))?;

    String::from_utf8(decrypted).map_err(|e| AppError::Internal(format!("UTF8 decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_key() -> [u8; 32] {
        derive_key("test-secret")
    }

    fn sample_secret() -> VaultSecret {
        VaultSecret {
            id: "i1".into(),
            name: "GitHub".into(),
            username: Some("octocat".into()),
            password: Some("ghp_xxx".into()),
            notes: None,
            fields: {
                let mut m = HashMap::new();
                m.insert("api_key".into(), "ghp_custom".into());
                m
            },
        }
    }

    #[test]
    fn project_target_prefix_spreads_all_fields() {
        let secret = sample_secret();
        let target = CredentialTarget::Prefix {
            env_var_prefix: "GH".into(),
        };
        let vars: std::collections::HashMap<_, _> = project_target(&secret, &target)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(vars.get("GH_USERNAME").map(String::as_str), Some("octocat"));
        assert_eq!(vars.get("GH_PASSWORD").map(String::as_str), Some("ghp_xxx"));
        assert_eq!(
            vars.get("GH_API_KEY").map(String::as_str),
            Some("ghp_custom")
        );
    }

    #[test]
    fn project_target_prefix_includes_all_custom_fields() {
        let secret = VaultSecret {
            id: "ha1".into(),
            name: "Home Assistant".into(),
            username: None,
            password: None,
            notes: None,
            fields: {
                let mut m = HashMap::new();
                m.insert("hostname".into(), "https://ha.example.com".into());
                m.insert("type".into(), "bearer".into());
                m.insert("credential".into(), "tok_secret_123".into());
                m
            },
        };
        let target = CredentialTarget::Prefix {
            env_var_prefix: "HOME_ASSISTANT".into(),
        };
        let vars: HashMap<_, _> = project_target(&secret, &target)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(vars.len(), 3);
        assert_eq!(
            vars.get("HOME_ASSISTANT_HOSTNAME").map(String::as_str),
            Some("https://ha.example.com")
        );
        assert_eq!(
            vars.get("HOME_ASSISTANT_TYPE").map(String::as_str),
            Some("bearer")
        );
        assert_eq!(
            vars.get("HOME_ASSISTANT_CREDENTIAL").map(String::as_str),
            Some("tok_secret_123")
        );
    }

    #[test]
    fn project_target_single_projects_password_only() {
        let secret = sample_secret();
        let target = CredentialTarget::Single {
            env_var: "GITHUB_TOKEN".into(),
            field: VaultField::Password,
        };
        let vars = project_target(&secret, &target).unwrap();
        assert_eq!(
            vars,
            vec![("GITHUB_TOKEN".to_string(), "ghp_xxx".to_string())]
        );
    }

    #[test]
    fn project_target_single_projects_username() {
        let secret = sample_secret();
        let target = CredentialTarget::Single {
            env_var: "GH_USER".into(),
            field: VaultField::Username,
        };
        let vars = project_target(&secret, &target).unwrap();
        assert_eq!(vars, vec![("GH_USER".to_string(), "octocat".to_string())]);
    }

    #[test]
    fn project_target_single_projects_custom_field() {
        let secret = sample_secret();
        let target = CredentialTarget::Single {
            env_var: "API_KEY".into(),
            field: VaultField::Custom {
                name: "api_key".into(),
            },
        };
        let vars = project_target(&secret, &target).unwrap();
        assert_eq!(
            vars,
            vec![("API_KEY".to_string(), "ghp_custom".to_string())]
        );
    }

    #[test]
    fn project_target_single_resolves_map_fields_using_picker_names() {
        let secret = VaultSecret {
            id: "managed".into(),
            name: "Composite".into(),
            username: None,
            password: None,
            notes: None,
            fields: HashMap::from([
                ("password".into(), "token".into()),
                ("service account".into(), "account".into()),
            ]),
        };
        for (field, expected) in [
            (VaultField::Password, "token"),
            (
                VaultField::Custom {
                    name: "SERVICE_ACCOUNT".into(),
                },
                "account",
            ),
        ] {
            assert_eq!(
                project_target(
                    &secret,
                    &CredentialTarget::Single {
                        env_var: "SELECTED".into(),
                        field,
                    }
                )
                .unwrap(),
                vec![("SELECTED".into(), expected.into())],
            );
        }
    }

    #[test]
    fn project_target_single_rejects_missing_when_field_missing() {
        let mut secret = sample_secret();
        secret.password = None;
        let target = CredentialTarget::Single {
            env_var: "GITHUB_TOKEN".into(),
            field: VaultField::Password,
        };
        assert!(project_target(&secret, &target).is_err());
    }

    #[test]
    fn project_target_single_rejects_missing_for_unknown_custom_field() {
        let secret = sample_secret();
        let target = CredentialTarget::Single {
            env_var: "X".into(),
            field: VaultField::Custom {
                name: "nonexistent".into(),
            },
        };
        assert!(project_target(&secret, &target).is_err());
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
