pub mod copilot;
pub mod openai_codex;
pub mod static_secret;

#[cfg(test)]
mod tests;

use super::{ManagedVault, Status};
use crate::core::error::AppError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    NoCache,
    Until(DateTime<Utc>),
    UntilChanged,
}

pub trait SecretEnv: std::any::Any + Send + Sync {
    fn to_env(&self) -> Result<HashMap<String, String>, AppError>;
}

#[derive(Clone)]
pub struct ResolvedSecret<T> {
    pub credentials: T,
    pub expires_at: Option<DateTime<Utc>>,
    pub cache: CachePolicy,
}

impl<T: SecretEnv> ResolvedSecret<T> {
    pub fn to_env(&self) -> Result<HashMap<String, String>, AppError> {
        export(&self.credentials, self.expires_at)
    }

    pub(crate) fn erase(self) -> ErasedSecret {
        ErasedSecret {
            credentials: Arc::new(self.credentials),
            expires_at: self.expires_at,
            cache: self.cache,
            identity: Uuid::new_v4(),
        }
    }
}

impl<T> std::fmt::Debug for ResolvedSecret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSecret")
            .field("credentials", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("cache", &self.cache)
            .finish()
    }
}

fn export(
    secret: &dyn SecretEnv,
    expiry: Option<DateTime<Utc>>,
) -> Result<HashMap<String, String>, AppError> {
    let mut fields = secret
        .to_env()
        .map_err(|_| AppError::Validation("managed credential could not be exported".into()))?;
    if let Some(expiry) = expiry {
        fields.insert("EXPIRES_AT".into(), expiry.to_rfc3339());
    }
    Ok(fields)
}

/// Heterogeneous cache entry. Secret values are never formatted for diagnostics.
#[derive(Clone)]
pub struct ErasedSecret {
    credentials: Arc<dyn SecretEnv>,
    pub expires_at: Option<DateTime<Utc>>,
    pub cache: CachePolicy,
    pub(crate) identity: Uuid,
}

impl ErasedSecret {
    pub fn credentials<T: SecretEnv>(&self) -> Result<&T, AppError> {
        (self.credentials.as_ref() as &dyn std::any::Any)
            .downcast_ref::<T>()
            .ok_or_else(|| AppError::Validation("managed credential type mismatch".into()))
    }

    pub fn to_env(&self) -> Result<HashMap<String, String>, AppError> {
        export(self.credentials.as_ref(), self.expires_at)
    }
}

impl std::fmt::Debug for ErasedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedSecret")
            .field("credentials", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("cache", &self.cache)
            .finish()
    }
}

#[async_trait]
pub trait ManagedIntegration: Send + Sync {
    type Credentials: SecretEnv;

    async fn get_secret(
        &self,
        doc: Value,
        ctx: &mut SecretContext,
    ) -> Result<ResolvedSecret<Self::Credentials>, AppError>;
}

#[async_trait]
pub trait RegisteredIntegration: Send + Sync {
    async fn resolve_erased(
        &self,
        doc: Value,
        ctx: &mut SecretContext,
    ) -> Result<ErasedSecret, AppError>;
}

#[async_trait]
impl<I: ManagedIntegration> RegisteredIntegration for I {
    async fn resolve_erased(
        &self,
        doc: Value,
        ctx: &mut SecretContext,
    ) -> Result<ErasedSecret, AppError> {
        Ok(self.get_secret(doc, ctx).await?.erase())
    }
}

/// Build the startup registrations, rejecting empty or duplicate identifiers.
pub fn register(
    entries: impl IntoIterator<Item = (String, Arc<dyn RegisteredIntegration>)>,
) -> Result<HashMap<String, Arc<dyn RegisteredIntegration>>, AppError> {
    let mut integrations = HashMap::new();
    for (name, integration) in entries {
        if name.is_empty() || integrations.insert(name, integration).is_some() {
            return Err(AppError::Validation(
                "duplicate or empty managed integration identifier".into(),
            ));
        }
    }
    Ok(integrations)
}

/// A resolution-scoped capability. Integrations cannot construct or retarget it.
pub struct SecretContext {
    vault: ManagedVault,
    status: Status,
    failed: bool,
}

impl SecretContext {
    pub(crate) fn new(vault: ManagedVault, status: Status) -> Self {
        Self {
            vault,
            status,
            failed: false,
        }
    }

    pub(crate) fn status(&self) -> &Status {
        &self.status
    }

    pub fn credential_id(&self) -> Uuid {
        self.status.item_id
    }

    pub fn version(&self) -> Uuid {
        self.status.version
    }

    pub async fn update_secret(&mut self, doc: Value) -> Result<(), AppError> {
        if self.failed {
            return Err(AppError::Conflict(
                "credential update already failed".into(),
            ));
        }
        match self
            .vault
            .replace_by_id(
                self.status.item_id,
                self.status.version,
                &self.status.integration,
                self.status.metadata.clone(),
                doc,
            )
            .await
        {
            Ok(status) => {
                self.status = status;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub(crate) async fn verify(&self) -> Result<(), AppError> {
        let current = self.vault.status_by_id(self.credential_id()).await?;
        if self.failed
            || !current.is_some_and(|s| {
                !s.removed
                    && s.version == self.status.version
                    && s.generation == self.status.generation
            })
        {
            return Err(AppError::Conflict(
                "managed credential changed during resolution".into(),
            ));
        }
        Ok(())
    }
}

pub fn registered() -> HashMap<String, Arc<dyn RegisteredIntegration>> {
    register([
        (
            static_secret::ID.into(),
            Arc::new(static_secret::StaticSecretIntegration) as Arc<dyn RegisteredIntegration>,
        ),
        (
            openai_codex::ID.into(),
            Arc::new(openai_codex::OpenAiCodexIntegration::default())
                as Arc<dyn RegisteredIntegration>,
        ),
        (
            copilot::ID.into(),
            Arc::new(copilot::CopilotIntegration::default()) as Arc<dyn RegisteredIntegration>,
        ),
    ])
    .expect("unique built-in managed integrations")
}
