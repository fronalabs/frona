use std::sync::Arc;

use chrono::{Duration, Utc};
use rand::RngExt;

use crate::chat::broadcast::{BroadcastService, EntityAction};
use crate::core::config::Config;
use crate::core::error::AppError;
use crate::core::principal::Principal;
use crate::core::supervisor::Supervisor;
use crate::credential::vault::models::{BindingScope, CredentialTarget};
use crate::credential::vault::service::VaultService;
use crate::tool::mcp::models::CredentialBinding;

use super::models::{
    Channel, ChannelManifest, ChannelStatus, ConfigRef, CreateChannelRequest, UpdateChannelRequest,
    UserAddress,
};
use super::registry::ChannelRegistry;
use super::repository::ChannelRepository;

const PAIRING_TTL: Duration = Duration::minutes(5);
const PAIR_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const PAIR_CODE_LEN: usize = 6;

pub fn resolve_config_default(config: &Config, r: &ConfigRef) -> Option<String> {
    let v = serde_json::to_value(config).ok()?;
    v.get(&r.section)?
        .get(&r.field)?
        .as_str()
        .map(|s| s.to_string())
}

#[derive(Clone)]
pub struct ChannelService {
    repo: Arc<dyn ChannelRepository>,
    registry: Arc<ChannelRegistry>,
    vault: Arc<VaultService>,
    broadcast: BroadcastService,
    config: Arc<Config>,
}

impl ChannelService {
    pub fn new(
        repo: Arc<dyn ChannelRepository>,
        registry: Arc<ChannelRegistry>,
        vault: Arc<VaultService>,
        broadcast: BroadcastService,
        config: Arc<Config>,
    ) -> Self {
        Self {
            repo,
            registry,
            vault,
            broadcast,
            config,
        }
    }

    pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<Channel>, AppError> {
        self.repo.find_by_user(user_id).await
    }

    pub async fn find_by_space(&self, space_id: &str) -> Result<Option<Channel>, AppError> {
        self.repo.find_by_space(space_id).await
    }

    pub async fn find_active(&self) -> Result<Vec<Channel>, AppError> {
        self.repo.find_active().await
    }

    pub async fn find_by_id(&self, channel_id: &str) -> Result<Channel, AppError> {
        self.repo
            .find_by_id(channel_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("channel {channel_id} not found")))
    }

    async fn allocate_channel_handle(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<crate::core::Handle, AppError> {
        let taken: std::collections::HashSet<String> = self
            .repo
            .find_by_user(user_id)
            .await?
            .into_iter()
            .map(|c| c.handle.to_string())
            .collect();
        let base = crate::core::Handle::try_new(provider).map_err(|e| {
            AppError::Validation(format!(
                "provider '{provider}' is not a valid handle base: {e}"
            ))
        })?;
        if !taken.contains(base.as_str()) {
            return Ok(base);
        }
        for i in 2..100u32 {
            let candidate = format!("{}-{i}", base.as_str());
            if !taken.contains(&candidate) {
                return crate::core::Handle::try_new(candidate)
                    .map_err(|e| AppError::Internal(format!("generated handle invalid: {e}")));
            }
        }
        Err(AppError::Validation(format!(
            "exhausted handle suffixes for provider '{provider}'"
        )))
    }

    pub async fn find_owned(&self, user_id: &str, channel_id: &str) -> Result<Channel, AppError> {
        let channel = self.find_by_id(channel_id).await?;
        if channel.user_id != user_id {
            return Err(AppError::Forbidden("not your channel".into()));
        }
        Ok(channel)
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateChannelRequest,
    ) -> Result<Channel, AppError> {
        let factory = self.registry.get_factory(&req.provider).ok_or_else(|| {
            AppError::Validation(format!("unknown channel provider {:?}", req.provider))
        })?;
        let manifest = factory.manifest();

        if let Some(existing) = self.repo.find_by_space(&req.space_id).await? {
            return Err(AppError::Validation(format!(
                "space {:?} already has a {:?} channel installed — uninstall it before installing {:?}",
                req.space_id, existing.provider, req.provider,
            )));
        }

        let missing =
            missing_required_fields(&manifest, &req.config, &req.credentials, &self.config);
        // Created disabled + Disconnected; the operator enables via `start` (the
        // enable-gate re-checks config). `error_message` is informational for the UI.
        let initial_error = if missing.is_empty() {
            None
        } else {
            Some(format!("missing required field(s): {}", missing.join(", ")))
        };

        let now = Utc::now();
        let handle = self.allocate_channel_handle(user_id, &req.provider).await?;
        let channel = Channel {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            handle,
            space_id: req.space_id,
            provider: req.provider,
            agent_id: req.agent_id,
            config: req.config,
            dispatch_mode: req.dispatch_mode,
            status: ChannelStatus::Disconnected,
            enabled: false,
            error_message: initial_error,
            last_started_at: None,
            user_address: None,
            setup: None,
            created_at: now,
            updated_at: now,
            webhook_url: None,
        };
        let persisted = self.repo.create(&channel).await?;

        let principal = Principal::channel(&persisted.id);
        self.verify_grants(user_id, &req.credentials, &principal)
            .await?;
        self.write_bindings(user_id, &persisted.id, req.credentials)
            .await?;

        self.broadcast.broadcast_entity_updated(
            user_id,
            "channel",
            &persisted.id,
            EntityAction::Created,
            Some(persisted.space_id.clone()),
            None,
        );
        Ok(persisted)
    }

    pub async fn update(
        &self,
        user_id: &str,
        channel_id: &str,
        req: UpdateChannelRequest,
    ) -> Result<Channel, AppError> {
        let mut channel = self.find_owned(user_id, channel_id).await?;

        if let Some(agent_id) = req.agent_id {
            channel.agent_id = agent_id;
        }
        if let Some(config) = req.config {
            channel.config = config;
        }

        if let Some(credentials) = &req.credentials {
            let principal = Principal::channel(&channel.id);
            self.verify_grants(user_id, credentials, &principal).await?;
            self.vault
                .delete_bindings_for_principal(user_id, &principal)
                .await?;
            self.write_bindings(user_id, &channel.id, credentials.clone())
                .await?;
        }

        // Refresh the informational missing-config message while disabled.
        if !channel.enabled {
            let missing = self.missing_required(&channel).await?;
            channel.error_message = if missing.is_empty() {
                None
            } else {
                Some(format!("missing required field(s): {}", missing.join(", ")))
            };
        }

        channel.updated_at = Utc::now();
        let persisted = self.repo.update(&channel).await?;

        self.broadcast.broadcast_entity_updated(
            user_id,
            "channel",
            &persisted.id,
            EntityAction::Updated,
            Some(persisted.space_id.clone()),
            None,
        );
        Ok(persisted)
    }

    pub async fn delete(
        &self,
        state: &crate::core::state::AppState,
        user_id: &str,
        channel_id: &str,
    ) -> Result<(), AppError> {
        let channel = self.find_owned(user_id, channel_id).await?;

        // Fire-and-forget: `run_outbound` runs `on_disconnect` on cancel.
        let _ = state.channel_supervisor.stop(&channel.id).await;

        if let Some(user) = state.user_service.find_by_id(&channel.user_id).await? {
            let dir = super::supervisor::channel_data_dir(
                &state.storage_service,
                &user.handle,
                &channel.handle,
            );
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    channel_id = %channel.id,
                    dir = %dir.display(),
                    error = %e,
                    "failed to remove channel data dir",
                ),
            }
        }

        let principal = Principal::channel(&channel.id);
        self.vault
            .delete_bindings_for_principal(user_id, &principal)
            .await?;
        self.repo.delete(&channel.id).await?;
        self.broadcast.broadcast_entity_updated(
            user_id,
            "channel",
            &channel.id,
            EntityAction::Deleted,
            Some(channel.space_id.clone()),
            None,
        );
        Ok(())
    }

    /// Enable-gate: sets intent `enabled = true` (refusing while required config is
    /// missing) and kicks the (idempotent) supervisor directly — so this doubles as
    /// "retry" for a terminally-`Failed` channel whose supervisor has exited. The
    /// supervisor owns all `status` writes from here.
    pub async fn start(
        &self,
        state: &crate::core::state::AppState,
        user_id: &str,
        channel_id: &str,
    ) -> Result<Channel, AppError> {
        let channel = self.find_owned(user_id, channel_id).await?;
        let missing = self.missing_required(&channel).await?;
        if !missing.is_empty() {
            let msg = format!("missing required field(s): {}", missing.join(", "));
            self.set_enabled(channel_id, false, Some(msg.clone()))
                .await?;
            return Err(AppError::Validation(msg));
        }
        self.set_enabled(channel_id, true, None).await?;
        let _ = state.channel_supervisor.start(channel_id).await;
        self.find_by_id(channel_id).await
    }

    /// Sets intent `enabled = false` and cancels the supervisor, which writes the
    /// final `Disconnected` status and clears the QR overlay on its way out.
    pub async fn stop(
        &self,
        state: &crate::core::state::AppState,
        user_id: &str,
        channel_id: &str,
    ) -> Result<Channel, AppError> {
        let channel = self.find_owned(user_id, channel_id).await?;
        if !channel.enabled {
            return Ok(channel);
        }
        self.set_enabled(channel_id, false, None).await?;
        let _ = state.channel_supervisor.stop(channel_id).await;
        self.find_by_id(channel_id).await
    }

    /// Writes the intent axis (and an informational `error_message`) + broadcasts.
    /// Never touches `status` — that is the supervisor's alone.
    async fn set_enabled(
        &self,
        channel_id: &str,
        enabled: bool,
        error: Option<String>,
    ) -> Result<(), AppError> {
        let mut channel = self.find_by_id(channel_id).await?;
        channel.enabled = enabled;
        channel.error_message = error;
        channel.updated_at = Utc::now();
        self.repo.update(&channel).await?;
        self.broadcast_update(&channel, EntityAction::Updated);
        Ok(())
    }

    pub async fn mark_status(
        &self,
        channel_id: &str,
        status: ChannelStatus,
        error: Option<String>,
    ) -> Result<(), AppError> {
        let mut channel = self.find_by_id(channel_id).await?;
        channel.status = status;
        channel.error_message = error;
        if matches!(status, ChannelStatus::Connected) {
            channel.last_started_at = Some(Utc::now());
        }
        channel.updated_at = Utc::now();
        let user_id = channel.user_id.clone();
        let space_id = channel.space_id.clone();
        let id = channel.id.clone();
        self.repo.update(&channel).await?;
        self.broadcast.broadcast_entity_updated(
            &user_id,
            "channel",
            &id,
            EntityAction::Updated,
            Some(space_id),
            None,
        );
        Ok(())
    }

    pub async fn resolve_config(&self, channel: &Channel) -> Result<serde_json::Value, AppError> {
        let factory = self
            .registry
            .get_factory(&channel.provider)
            .ok_or_else(|| {
                AppError::Validation(format!("unknown channel provider {:?}", channel.provider))
            })?;
        let manifest = factory.manifest();

        let mut out = serde_json::Map::new();
        for field in &manifest.config_fields {
            if let Some(r) = &field.default_from
                && let Some(v) = resolve_config_default(&self.config, r)
            {
                out.insert(field.name.clone(), serde_json::Value::String(v));
            }
        }
        for (k, v) in &channel.config {
            out.insert(k.clone(), serde_json::Value::String(v.clone()));
        }

        let principal = Principal::channel(&channel.id);
        for (key, value) in self
            .vault
            .resolve_env(&channel.user_id, &principal, None)
            .await?
        {
            out.insert(key, serde_json::Value::String(value));
        }

        for field in &manifest.config_fields {
            if field.is_required && !out.contains_key(&field.name) {
                return Err(AppError::Validation(format!(
                    "channel field {:?} is required but unresolved — supply it in config or bind a vault credential",
                    field.name,
                )));
            }
        }
        Ok(serde_json::Value::Object(out))
    }

    pub fn list_manifests_with_resolved_defaults(&self) -> Vec<ChannelManifest> {
        let mut manifests = self.registry.list_manifests();
        for manifest in &mut manifests {
            for field in &mut manifest.config_fields {
                if field.is_secret {
                    continue;
                }
                if let Some(r) = &field.default_from {
                    field.default_resolved = resolve_config_default(&self.config, r);
                }
            }
        }
        manifests
    }

    pub async fn missing_required(&self, channel: &Channel) -> Result<Vec<String>, AppError> {
        let factory = self
            .registry
            .get_factory(&channel.provider)
            .ok_or_else(|| {
                AppError::Validation(format!("unknown channel provider {:?}", channel.provider))
            })?;
        let manifest = factory.manifest();
        let bindings = self
            .vault
            .list_bindings_for_principal(&channel.user_id, &Principal::channel(&channel.id))
            .await?;
        let virtual_bindings: Vec<CredentialBinding> = bindings
            .into_iter()
            .map(|b| CredentialBinding {
                connection_id: b.connection_id,
                vault_item_id: b.vault_item_id,
                env_var: match &b.target {
                    CredentialTarget::Single { env_var, .. } => env_var.clone(),
                    _ => String::new(),
                },
                field: match b.target {
                    CredentialTarget::Single { field, .. } => field,
                    _ => crate::credential::vault::models::VaultField::Password,
                },
            })
            .collect();
        Ok(missing_required_fields(
            &manifest,
            &channel.config,
            &virtual_bindings,
            &self.config,
        ))
    }

    async fn write_bindings(
        &self,
        user_id: &str,
        channel_id: &str,
        bindings: Vec<CredentialBinding>,
    ) -> Result<(), AppError> {
        let principal = Principal::channel(channel_id);
        for binding in bindings {
            self.vault
                .create_binding(
                    user_id,
                    principal.clone(),
                    &binding.env_var,
                    &binding.connection_id,
                    &binding.vault_item_id,
                    CredentialTarget::Single {
                        env_var: binding.env_var.clone(),
                        field: binding.field,
                    },
                    BindingScope::Durable,
                    None,
                )
                .await?;
        }
        Ok(())
    }

    async fn verify_grants(
        &self,
        user_id: &str,
        bindings: &[CredentialBinding],
        principal: &Principal,
    ) -> Result<(), AppError> {
        for binding in bindings {
            let ok = self
                .vault
                .has_grant_for_item(
                    user_id,
                    principal,
                    &binding.connection_id,
                    &binding.vault_item_id,
                )
                .await?;
            if !ok {
                return Err(AppError::Forbidden(format!(
                    "no grant for vault item {} in connection {} — approve it before installing",
                    binding.vault_item_id, binding.connection_id,
                )));
            }
        }
        Ok(())
    }

    pub async fn initiate_pairing(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> Result<String, AppError> {
        let mut channel = self.find_owned(user_id, channel_id).await?;
        let now = Utc::now();
        let code = generate_pair_code();
        let prior_address = channel
            .user_address
            .as_ref()
            .and_then(|ua| ua.address.clone());
        channel.user_address = Some(UserAddress {
            address: prior_address,
            pairing_code: Some(code.clone()),
            pairing_initiated_at: Some(now),
            paired_at: channel.user_address.and_then(|ua| ua.paired_at),
        });
        // Pairing is an overlay on `user_address.pairing_code` — it does NOT touch
        // the connection `status` (the socket stays live to receive the code).
        channel.updated_at = now;
        self.repo.update(&channel).await?;
        self.broadcast_update(&channel, EntityAction::Updated);
        Ok(code)
    }

    pub async fn cancel_pairing(&self, user_id: &str, channel_id: &str) -> Result<(), AppError> {
        let channel = self.find_owned(user_id, channel_id).await?;
        if pairing_pending(&channel) {
            self.revert_pairing(channel).await?;
        }
        Ok(())
    }

    /// Supervisor-owned setup (QR) overlay write. No `status` change — the
    /// supervisor drives connection status separately.
    pub async fn set_setup(
        &self,
        channel_id: &str,
        mut setup: super::models::SetupConfig,
    ) -> Result<(), AppError> {
        let mut channel = self
            .repo
            .find_by_id(channel_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("channel {channel_id} not found")))?;
        let now = Utc::now();
        setup.initiated_at = Some(now);
        channel.setup = Some(setup);
        channel.updated_at = now;
        self.repo.update(&channel).await?;
        self.broadcast_update(&channel, EntityAction::Updated);
        Ok(())
    }

    /// Clears the setup (QR) overlay. No `status` change.
    pub async fn clear_setup(&self, channel_id: &str) -> Result<(), AppError> {
        let mut channel = self
            .repo
            .find_by_id(channel_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("channel {channel_id} not found")))?;
        if channel.setup.is_none() {
            return Ok(());
        }
        channel.setup = None;
        channel.updated_at = Utc::now();
        self.repo.update(&channel).await?;
        self.broadcast_update(&channel, EntityAction::Updated);
        Ok(())
    }

    pub async fn try_redeem_pairing(
        &self,
        channel_id: &str,
        sender_address: &str,
        message_body: &str,
    ) -> Result<bool, AppError> {
        let mut channel = self
            .repo
            .find_by_id(channel_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("channel {channel_id} not found")))?;
        let Some(ua) = channel.user_address.as_ref() else {
            return Ok(false);
        };
        let Some(expected) = ua.pairing_code.as_deref() else {
            return Ok(false);
        };
        if message_body.trim() != expected {
            return Ok(false);
        }
        let now = Utc::now();
        // Redemption clears the pending overlay and records the durable binding.
        // Connection `status` is untouched (owned by the supervisor).
        channel.user_address = Some(UserAddress {
            address: Some(sender_address.to_string()),
            pairing_code: None,
            pairing_initiated_at: None,
            paired_at: Some(now),
        });
        channel.updated_at = now;
        self.repo.update(&channel).await?;
        self.broadcast_update(&channel, EntityAction::Updated);
        Ok(true)
    }

    pub async fn revert_expired_pairings(&self) -> Result<u64, AppError> {
        let now = Utc::now();
        let cutoff = now - PAIRING_TTL;
        let pending = self.repo.find_pairing_pending().await?;
        let mut reverted = 0u64;
        for channel in pending {
            let initiated = channel
                .user_address
                .as_ref()
                .and_then(|ua| ua.pairing_initiated_at);
            if initiated.is_some_and(|t| t < cutoff) {
                self.revert_pairing(channel).await?;
                reverted += 1;
            }
        }
        Ok(reverted)
    }

    pub async fn revert_orphaned_pairings(&self) -> Result<u64, AppError> {
        let pending = self.repo.find_pairing_pending().await?;
        let count = pending.len() as u64;
        for channel in pending {
            self.revert_pairing(channel).await?;
        }
        Ok(count)
    }

    /// Clears the pending pairing overlay, preserving any already-bound address.
    /// Does NOT touch connection `status` (owned by the supervisor).
    async fn revert_pairing(&self, mut channel: Channel) -> Result<(), AppError> {
        let prior_address = channel
            .user_address
            .as_ref()
            .and_then(|ua| ua.address.clone());
        let prior_paired_at = channel.user_address.as_ref().and_then(|ua| ua.paired_at);
        channel.user_address = if prior_address.is_some() {
            Some(UserAddress {
                address: prior_address,
                pairing_code: None,
                pairing_initiated_at: None,
                paired_at: prior_paired_at,
            })
        } else {
            None
        };
        channel.updated_at = Utc::now();
        self.repo.update(&channel).await?;
        self.broadcast_update(&channel, EntityAction::Updated);
        Ok(())
    }

    fn broadcast_update(&self, channel: &Channel, action: EntityAction) {
        self.broadcast.broadcast_entity_updated(
            &channel.user_id,
            "channel",
            &channel.id,
            action,
            Some(channel.space_id.clone()),
            None,
        );
    }
}

/// True while a pairing round-trip is in flight (overlay present), independent
/// of connection status.
fn pairing_pending(channel: &Channel) -> bool {
    channel
        .user_address
        .as_ref()
        .and_then(|ua| ua.pairing_code.as_ref())
        .is_some()
}

fn generate_pair_code() -> String {
    let mut rng = rand::rng();
    (0..PAIR_CODE_LEN)
        .map(|_| {
            let idx = rng.random_range(0..PAIR_CODE_ALPHABET.len());
            PAIR_CODE_ALPHABET[idx] as char
        })
        .collect()
}

fn missing_required_fields(
    manifest: &ChannelManifest,
    config: &std::collections::BTreeMap<String, String>,
    credentials: &[CredentialBinding],
    server_config: &Config,
) -> Vec<String> {
    let mut missing = Vec::new();
    for field in &manifest.config_fields {
        if !field.is_required {
            continue;
        }
        let has_default = field
            .default_from
            .as_ref()
            .and_then(|r| resolve_config_default(server_config, r))
            .is_some();
        let satisfied = has_default
            || config.contains_key(&field.name)
            || (field.is_secret && credentials.iter().any(|b| b.env_var == field.name));
        if !satisfied {
            missing.push(field.name.clone());
        }
    }
    missing
}
