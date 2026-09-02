use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::AppError;
use crate::core::runtime_config::RuntimeConfigStore;

use super::PkmService;
use super::vault::VaultScope;

const KEY_PREFIX: &str = "pkm.reset.";
const STATE_RETRIES: usize = 8;
const RESET_WAIT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PkmResetState {
    Pending,
    Running,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PkmResetStatus {
    pub request_id: String,
    pub state: PkmResetState,
    pub requested_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub(crate) struct PkmResetStateStore {
    runtime: RuntimeConfigStore,
}

impl PkmResetStateStore {
    pub(crate) fn new(runtime: RuntimeConfigStore) -> Self {
        Self { runtime }
    }

    fn key(user_id: &str) -> String {
        format!("{KEY_PREFIX}{user_id}")
    }

    pub(crate) async fn status(&self, user_id: &str) -> Result<Option<PkmResetStatus>, AppError> {
        self.runtime.get(&Self::key(user_id)).await
    }

    pub(crate) async fn request(&self, user_id: &str) -> Result<PkmResetStatus, AppError> {
        let key = Self::key(user_id);
        for _ in 0..STATE_RETRIES {
            let current: Option<PkmResetStatus> = self.runtime.get(&key).await?;
            if let Some(status) = &current
                && matches!(
                    status.state,
                    PkmResetState::Pending | PkmResetState::Running
                )
            {
                return Ok(status.clone());
            }
            let requested = PkmResetStatus {
                request_id: crate::core::repository::new_id(),
                state: PkmResetState::Pending,
                requested_at: Utc::now(),
                started_at: None,
                error: None,
            };
            if self
                .runtime
                .compare_exchange(&key, current.as_ref(), Some(&requested))
                .await?
            {
                return Ok(requested);
            }
        }
        Err(AppError::Conflict(
            "PKM reset state changed concurrently; try again".into(),
        ))
    }

    pub(crate) async fn claim(&self, user_id: &str, request_id: &str) -> Result<bool, AppError> {
        let key = Self::key(user_id);
        let Some(current): Option<PkmResetStatus> = self.runtime.get(&key).await? else {
            return Ok(false);
        };
        if current.request_id != request_id || current.state != PkmResetState::Pending {
            return Ok(false);
        }
        let running = PkmResetStatus {
            state: PkmResetState::Running,
            started_at: Some(Utc::now()),
            error: None,
            ..current.clone()
        };
        self.runtime
            .compare_exchange(&key, Some(&current), Some(&running))
            .await
    }

    pub(crate) async fn fail(
        &self,
        user_id: &str,
        request_id: &str,
        error: String,
    ) -> Result<bool, AppError> {
        let key = Self::key(user_id);
        let Some(current): Option<PkmResetStatus> = self.runtime.get(&key).await? else {
            return Ok(false);
        };
        if current.request_id != request_id {
            return Ok(false);
        }
        let failed = PkmResetStatus {
            state: PkmResetState::Failed,
            error: Some(error),
            ..current.clone()
        };
        self.runtime
            .compare_exchange(&key, Some(&current), Some(&failed))
            .await
    }

    pub(crate) async fn complete(&self, user_id: &str, request_id: &str) -> Result<bool, AppError> {
        let key = Self::key(user_id);
        let Some(current): Option<PkmResetStatus> = self.runtime.get(&key).await? else {
            return Ok(false);
        };
        if current.request_id != request_id || current.state != PkmResetState::Running {
            return Ok(false);
        }
        self.runtime
            .compare_exchange(&key, Some(&current), None)
            .await
    }

    pub(crate) async fn list(&self) -> Result<Vec<(String, PkmResetStatus)>, AppError> {
        self.runtime
            .list_prefix::<PkmResetStatus>(KEY_PREFIX)
            .await
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|(key, status)| {
                        key.strip_prefix(KEY_PREFIX)
                            .map(|user_id| (user_id.to_string(), status))
                    })
                    .collect()
            })
    }

    pub(crate) async fn requeue_interrupted(
        &self,
        user_id: &str,
        status: &PkmResetStatus,
    ) -> Result<bool, AppError> {
        if status.state != PkmResetState::Running {
            return Ok(false);
        }
        let pending = PkmResetStatus {
            state: PkmResetState::Pending,
            started_at: None,
            error: None,
            ..status.clone()
        };
        self.runtime
            .compare_exchange(&Self::key(user_id), Some(status), Some(&pending))
            .await
    }
}

impl PkmService {
    pub async fn request_reset(&self, user_id: &str) -> Result<PkmResetStatus, AppError> {
        let status = self.reset_state.request(user_id).await?;
        self.operations.mark_reset_pending(user_id);
        Ok(status)
    }

    pub async fn reset_status(&self, user_id: &str) -> Result<Option<PkmResetStatus>, AppError> {
        self.reset_state.status(user_id).await
    }

    pub async fn process_reset_request(&self, user_id: String, request_id: String) {
        match self.reset_state.claim(&user_id, &request_id).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::error!(%error, user = %user_id, "pkm reset: could not claim request");
                return;
            }
        }

        self.operations.mark_reset_pending(&user_id);
        let reset_guard = match tokio::time::timeout(
            std::time::Duration::from_secs(RESET_WAIT_SECS),
            self.operations.begin_reset(&user_id),
        )
        .await
        {
            Ok(guard) => guard,
            Err(_) => {
                self.fail_reset(
                    &user_id,
                    &request_id,
                    "Timed out while waiting for active PKM work to stop".into(),
                )
                .await;
                return;
            }
        };

        match self.reset_user(&user_id).await {
            Ok(()) => match self.reset_state.complete(&user_id, &request_id).await {
                Ok(true) => {
                    self.operations.clear_reset(&user_id);
                    tracing::info!(user = %user_id, "pkm reset: completed");
                }
                Ok(false) => {
                    tracing::error!(user = %user_id, "pkm reset: completion state was replaced")
                }
                Err(error) => {
                    tracing::error!(%error, user = %user_id, "pkm reset: could not complete request state")
                }
            },
            Err(error) => {
                self.fail_reset(&user_id, &request_id, error.to_string())
                    .await
            }
        }
        drop(reset_guard);
    }

    async fn reset_user(&self, user_id: &str) -> Result<(), AppError> {
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;
        let vault =
            VaultScope::resolve(&self.user_service, &self.storage, user_id, &user.handle).await?;
        self.repo.reset_user_derived_memory(user_id).await?;
        self.ontology_manager.evict_reasoned_graph(user_id);
        self.storage.delete_memory_directory(&vault)?;
        Ok(())
    }

    async fn fail_reset(&self, user_id: &str, request_id: &str, error: String) {
        tracing::error!(user = %user_id, %error, "pkm reset: failed");
        if let Err(state_error) = self.reset_state.fail(user_id, request_id, error).await {
            tracing::error!(error = %state_error, user = %user_id, "pkm reset: could not save failure state");
        }
    }

    pub(crate) async fn recover_reset_requests(&self) -> Result<(), AppError> {
        for (user_id, status) in self.reset_state.list().await? {
            self.operations.mark_reset_pending(&user_id);
            match status.state {
                PkmResetState::Failed => {}
                PkmResetState::Pending => self.spawn_reset(user_id, status.request_id),
                PkmResetState::Running => {
                    if self
                        .reset_state
                        .requeue_interrupted(&user_id, &status)
                        .await?
                    {
                        self.spawn_reset(user_id, status.request_id);
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn spawn_pending_reset_requests(
        &self,
    ) -> Result<std::collections::BTreeSet<String>, AppError> {
        let mut reset_users = std::collections::BTreeSet::new();
        for (user_id, status) in self.reset_state.list().await? {
            reset_users.insert(user_id.clone());
            self.operations.mark_reset_pending(&user_id);
            if status.state == PkmResetState::Pending {
                self.spawn_reset(user_id, status.request_id);
            }
        }
        Ok(reset_users)
    }

    pub fn spawn_reset(&self, user_id: String, request_id: String) {
        let service = self.clone();
        tokio::spawn(async move {
            service.process_reset_request(user_id, request_id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::Surreal;
    use surrealdb::engine::local::Mem;

    async fn store() -> PkmResetStateStore {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        PkmResetStateStore::new(RuntimeConfigStore::new(db))
    }

    #[tokio::test]
    async fn request_is_idempotent_and_only_one_worker_claims_it() {
        let store = store().await;
        let first = store.request("u1").await.unwrap();
        let repeated = store.request("u1").await.unwrap();
        assert_eq!(first.request_id, repeated.request_id);
        assert!(store.claim("u1", &first.request_id).await.unwrap());
        assert!(!store.claim("u1", &first.request_id).await.unwrap());
        assert_eq!(
            store.status("u1").await.unwrap().unwrap().state,
            PkmResetState::Running
        );
    }

    #[tokio::test]
    async fn stale_worker_cannot_change_a_retried_request() {
        let store = store().await;
        let failed = store.request("u1").await.unwrap();
        assert!(
            store
                .fail("u1", &failed.request_id, "failed".into())
                .await
                .unwrap()
        );
        let retry = store.request("u1").await.unwrap();
        assert_ne!(failed.request_id, retry.request_id);
        assert!(!store.complete("u1", &failed.request_id).await.unwrap());
        assert!(
            !store
                .fail("u1", &failed.request_id, "stale".into())
                .await
                .unwrap()
        );
        assert_eq!(
            store.status("u1").await.unwrap().unwrap().request_id,
            retry.request_id
        );
    }

    #[tokio::test]
    async fn interrupted_running_request_returns_to_pending() {
        let store = store().await;
        let requested = store.request("u1").await.unwrap();
        assert!(store.claim("u1", &requested.request_id).await.unwrap());
        let running = store.status("u1").await.unwrap().unwrap();
        assert!(store.requeue_interrupted("u1", &running).await.unwrap());
        let pending = store.status("u1").await.unwrap().unwrap();
        assert_eq!(pending.state, PkmResetState::Pending);
        assert_eq!(pending.request_id, requested.request_id);
    }
}
