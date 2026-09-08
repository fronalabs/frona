//! Interactive attempts own protocol state; stored credentials own renewable state.

use super::provider::*;
use crate::{
    core::error::AppError,
    credential::managed::{Key, ManagedVault, Status},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_PENDING_ATTEMPTS: usize = 64;
const MAX_RETAINED_RESULTS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginStatus {
    Pending,
    Validated,
    Failed,
    Expired,
    Cancelled,
}

#[derive(Clone)]
pub struct LoginTarget {
    pub vault: ManagedVault,
    pub key: Key,
    pub expected_id: Option<Uuid>,
    pub metadata: serde_json::Value,
}

#[derive(Serialize)]
pub struct LoginAttempt {
    pub id: Uuid,
    pub status: LoginStatus,
    pub challenge: Option<LoginChallenge>,
    pub credential_id: Option<Uuid>,
    pub error: Option<&'static str>,
}

struct Attempt {
    owner: String,
    vault_id: String,
    target_key: Key,
    target: Option<LoginTarget>,
    baseline: Option<(Uuid, Uuid, u64)>,
    session: Option<Box<dyn LoginSession>>,
    challenge: Option<LoginChallenge>,
    expires_at: DateTime<Utc>,
    status: LoginStatus,
    credential_id: Option<Uuid>,
}

impl Attempt {
    fn public(&self, id: Uuid) -> LoginAttempt {
        LoginAttempt {
            id,
            status: self.status,
            challenge: self.challenge.clone(),
            credential_id: self.credential_id,
            error: (self.status == LoginStatus::Failed)
                .then_some("Login failed or was superseded; start a new attempt"),
        }
    }

    async fn finish(&mut self, status: LoginStatus) -> Result<(), AppError> {
        self.status = status;
        self.session = None;
        self.challenge = None;
        self.target = None;
        self.baseline = None;
        Ok(())
    }
}

fn identity(status: Status) -> (Uuid, Uuid, u64) {
    (status.item_id, status.version, status.generation)
}

#[derive(Clone)]
pub struct ManagedLoginService {
    providers: Arc<HashMap<String, Arc<dyn LoginProvider>>>,
    attempts: Arc<Mutex<HashMap<Uuid, Arc<Mutex<Attempt>>>>>,
}

impl ManagedLoginService {
    pub fn new(
        providers: impl IntoIterator<Item = Arc<dyn LoginProvider>>,
    ) -> Result<Self, AppError> {
        let mut entries = HashMap::new();
        for provider in providers {
            if provider.id().is_empty() || entries.insert(provider.id().into(), provider).is_some()
            {
                return Err(AppError::Validation(
                    "duplicate or empty login provider ID".into(),
                ));
            }
        }
        Ok(Self {
            providers: Arc::new(entries),
            attempts: Default::default(),
        })
    }

    pub fn registered() -> Self {
        Self::new([
            Arc::new(super::openrouter::OpenRouterLogin::default()) as Arc<dyn LoginProvider>,
            Arc::new(super::openai_codex::OpenAiCodexLogin::default()) as Arc<dyn LoginProvider>,
            Arc::new(super::copilot::CopilotLogin::default()) as Arc<dyn LoginProvider>,
        ])
        .expect("unique built-in login providers")
    }

    pub fn available(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }

    /// The caller must authorize the target vault. The service binds every subsequent
    /// operation to that authenticated user and the exact target captured here.
    pub async fn start(
        &self,
        user_id: &str,
        provider: &str,
        target: LoginTarget,
    ) -> Result<LoginAttempt, AppError> {
        if user_id.is_empty() {
            return Err(AppError::Forbidden("login requires a user".into()));
        }
        target.vault.ensure_available().await?;
        let driver = self
            .providers
            .get(provider)
            .ok_or_else(|| AppError::Validation("login provider is unavailable".into()))?;
        let current = match target.expected_id {
            Some(id) => Some(
                target
                    .vault
                    .status_by_id(id)
                    .await?
                    .ok_or_else(|| AppError::NotFound("managed credential".into()))?,
            ),
            None => None,
        };
        let baseline = current.clone().map(identity);
        if current.filter(|s| !s.deleted).map(|s| s.item_id) != target.expected_id {
            return Err(AppError::Conflict(
                "login target was deleted or replaced".into(),
            ));
        }
        let started = tokio::time::timeout(Duration::from_secs(30), driver.start())
            .await
            .map_err(|_| AppError::Validation("login start timed out".into()))
            .and_then(|result| {
                result.map_err(|_| AppError::Validation("provider login could not start".into()))
            });
        let started = match started {
            Ok(started) => started,
            Err(error) => {
                return Err(error);
            }
        };
        let url = match &started.challenge {
            LoginChallenge::Redirect { url, .. } | LoginChallenge::DeviceCode { url, .. } => url,
        };
        if !reqwest::Url::parse(url).is_ok_and(|u| {
            u.scheme() == "https" && u.username().is_empty() && u.password().is_none()
        }) || started.expires_at <= Utc::now()
        {
            return Err(AppError::Validation(
                "provider returned an invalid login challenge".into(),
            ));
        }
        let mut attempts = self.attempts.lock().await;
        let mut terminal = Vec::new();
        let mut pending = 0;
        for (id, slot) in attempts.iter() {
            let mut attempt = slot.lock().await;
            if attempt.expires_at <= Utc::now() {
                attempt.finish(LoginStatus::Expired).await?;
            }
            // Supersession releases capacity before admitting the replacement.
            if attempt.owner == user_id
                && attempt.vault_id == target.vault.connection_id()
                && attempt.target_key == target.key
                && attempt.status == LoginStatus::Pending
            {
                attempt.finish(LoginStatus::Cancelled).await?;
            }
            if attempt.status == LoginStatus::Pending {
                pending += 1;
            } else {
                terminal.push((attempt.expires_at, *id));
            }
        }
        // On admission retain at most 64 non-secret results for polling/replay.
        // Between starts, finished pending work can bring the total to 128.
        // Terminal attempts hold no vault, session, or credential baseline.
        terminal.sort_unstable();
        let excess = terminal.len().saturating_sub(MAX_RETAINED_RESULTS);
        for (index, (expires_at, id)) in terminal.into_iter().enumerate() {
            if index < excess || expires_at <= Utc::now() {
                attempts.remove(&id);
            }
        }
        if pending >= MAX_PENDING_ATTEMPTS {
            return Err(AppError::Conflict("too many pending login attempts".into()));
        }
        let id = Uuid::new_v4();
        let attempt = Attempt {
            owner: user_id.into(),
            vault_id: target.vault.connection_id().into(),
            target_key: target.key.clone(),
            target: Some(target),
            baseline,
            session: Some(started.session),
            challenge: Some(started.challenge),
            expires_at: started
                .expires_at
                .min(Utc::now() + chrono::Duration::minutes(15)),
            status: LoginStatus::Pending,
            credential_id: None,
        };
        let result = attempt.public(id);
        attempts.insert(id, Arc::new(Mutex::new(attempt)));
        Ok(result)
    }

    async fn attempt(&self, user_id: &str, id: Uuid) -> Result<Arc<Mutex<Attempt>>, AppError> {
        let slot = self
            .attempts
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| AppError::NotFound("login attempt".into()))?;
        if slot.lock().await.owner != user_id {
            return Err(AppError::NotFound("login attempt".into()));
        }
        Ok(slot)
    }

    pub(crate) async fn check_vault(
        &self,
        user_id: &str,
        id: Uuid,
        connection_id: &str,
    ) -> Result<(), AppError> {
        let attempt = self.attempt(user_id, id).await?;
        if attempt.lock().await.vault_id != connection_id {
            return Err(AppError::NotFound("login attempt".into()));
        }
        Ok(())
    }

    pub async fn check_connection(
        &self,
        user_id: &str,
        id: Uuid,
        connection: &str,
    ) -> Result<(), AppError> {
        let attempt = self.attempt(user_id, id).await?;
        if attempt.lock().await.target_key.connection != connection {
            return Err(AppError::NotFound("login attempt".into()));
        }
        Ok(())
    }

    pub async fn cancel(&self, user_id: &str, id: Uuid) -> Result<LoginAttempt, AppError> {
        let slot = self.attempt(user_id, id).await?;
        let mut attempt = slot.lock().await;
        if attempt.status == LoginStatus::Pending {
            attempt.finish(LoginStatus::Cancelled).await?;
        }
        Ok(attempt.public(id))
    }

    pub(crate) async fn advance_authorized<F, Fut>(
        &self,
        user_id: &str,
        id: Uuid,
        completion: Option<&str>,
        authorize: F,
    ) -> Result<LoginAttempt, AppError>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<(), AppError>>,
    {
        if completion.is_some_and(|c| c.is_empty() || c.len() > 8192) {
            return Err(AppError::Validation("invalid login completion".into()));
        }
        let slot = self.attempt(user_id, id).await?;
        let mut attempt = slot.lock().await;
        if attempt.status != LoginStatus::Pending {
            if completion.is_some() {
                return Err(AppError::Conflict(
                    "login attempt is no longer pending".into(),
                ));
            }
            return Ok(attempt.public(id));
        }
        if attempt.expires_at <= Utc::now() {
            attempt.finish(LoginStatus::Expired).await?;
            return Ok(attempt.public(id));
        }
        let target = attempt
            .target
            .as_ref()
            .expect("pending login target")
            .clone();
        if let Err(error) = authorize(attempt.vault_id.clone()).await {
            attempt.finish(LoginStatus::Failed).await?;
            return Err(error);
        }
        target.vault.ensure_available().await?;
        let current = match target.expected_id {
            Some(id) => target.vault.status_by_id(id).await?.map(identity),
            None => None,
        };
        if current != attempt.baseline {
            attempt.finish(LoginStatus::Failed).await?;
            return Ok(attempt.public(id));
        }
        let progress = tokio::time::timeout(
            Duration::from_secs(30),
            attempt
                .session
                .as_mut()
                .expect("pending login session")
                .advance(completion),
        )
        .await;
        match progress {
            Ok(Ok(LoginProgress::Pending)) => {}
            Ok(Ok(LoginProgress::Authenticated(document))) => {
                if attempt.expires_at <= Utc::now() {
                    attempt.finish(LoginStatus::Expired).await?;
                    return Ok(attempt.public(id));
                }
                if let Err(error) = authorize(attempt.vault_id.clone()).await {
                    attempt.finish(LoginStatus::Failed).await?;
                    return Err(error);
                }
                let result = match attempt.baseline {
                    Some((id, version, _)) => {
                        target
                            .vault
                            .replace_by_id(
                                id,
                                version,
                                document.integration,
                                target.metadata.clone(),
                                document.secret,
                            )
                            .await
                    }
                    None => {
                        target
                            .vault
                            .create(
                                document.integration,
                                target.metadata.clone(),
                                document.secret,
                            )
                            .await
                    }
                };
                match result {
                    Ok(saved) => {
                        attempt.credential_id = Some(saved.item_id);
                        attempt.finish(LoginStatus::Validated).await?;
                    }
                    Err(_) => attempt.finish(LoginStatus::Failed).await?,
                }
            }
            _ => attempt.finish(LoginStatus::Failed).await?,
        }
        Ok(attempt.public(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::error::AppError,
        credential::managed::{Key, integration::*, resolver::ManagedResolver},
    };
    use chrono::Utc;
    use serde_json::json;
    use std::sync::Arc;

    impl ManagedLoginService {
        // Protocol/storage fixtures have no user repository. Production callers
        // use VaultService::advance_login with current user/vault authorization.
        pub(crate) async fn advance(
            &self,
            user: &str,
            id: Uuid,
            completion: Option<&str>,
        ) -> Result<LoginAttempt, AppError> {
            self.advance_authorized(user, id, completion, |_| async { Ok(()) })
                .await
        }
    }

    struct Provider;

    struct Session;

    #[async_trait::async_trait]
    impl LoginProvider for Provider {
        fn id(&self) -> &'static str {
            "fixture"
        }

        async fn start(&self) -> Result<StartedLogin, AppError> {
            Ok(StartedLogin {
                challenge: LoginChallenge::Redirect {
                    url: "https://login.example/authorize".into(),
                    message: None,
                },
                expires_at: Utc::now() + chrono::Duration::minutes(5),
                session: Box::new(Session),
            })
        }
    }

    #[async_trait::async_trait]
    impl LoginSession for Session {
        async fn advance(&mut self, code: Option<&str>) -> Result<LoginProgress, AppError> {
            match code {
                None => Ok(LoginProgress::Pending),
                Some("deny") => Ok(LoginProgress::Denied),
                Some(key) => Ok(LoginProgress::Authenticated(LoginDocument {
                    integration: "static",
                    secret: json!({"API_KEY":key,"ACCOUNT_ID":"account"}),
                })),
            }
        }
    }

    async fn fixture() -> (ManagedLoginService, LoginTarget) {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let vault =
            crate::credential::managed::test_support::vault(&db, "personal", "secret").await;
        (
            ManagedLoginService::new([Arc::new(Provider) as Arc<dyn LoginProvider>]).unwrap(),
            LoginTarget {
                vault,
                key: Key::new("account", "credential"),
                expected_id: None,
                metadata: json!({"name":"account"}),
            },
        )
    }

    #[tokio::test]
    async fn terminal_attempts_release_capacity_for_other_users() {
        for outcome in ["cancel", "deny", "success", "supersede", "expire"] {
            let (service, target) = fixture().await;
            for _ in 0..65 {
                let attempt = service
                    .start("user", "fixture", target.clone())
                    .await
                    .unwrap();
                match outcome {
                    "cancel" => {
                        service.cancel("user", attempt.id).await.unwrap();
                    }
                    "deny" => {
                        service
                            .advance("user", attempt.id, Some("deny"))
                            .await
                            .unwrap();
                    }
                    "success" => {
                        service
                            .advance("user", attempt.id, Some("fixture-key"))
                            .await
                            .unwrap();
                    }
                    "expire" => {
                        service
                            .attempt("user", attempt.id)
                            .await
                            .unwrap()
                            .lock()
                            .await
                            .expires_at = Utc::now();
                        service.advance("user", attempt.id, None).await.unwrap();
                    }
                    _ => {}
                }
            }
            for user in ["other", "admin"] {
                let mut target = target.clone();
                target.key = Key::new(user, "credential");
                service.start(user, "fixture", target).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn concurrent_pending_attempts_keep_limit_and_allow_supersession_at_capacity() {
        let (service, target) = fixture().await;
        let mut starts = tokio::task::JoinSet::new();
        for index in 0..64 {
            let service = service.clone();
            let mut target = target.clone();
            target.key = Key::new(format!("account-{index}"), "credential");
            starts.spawn(async move { service.start("user", "fixture", target).await });
        }
        let mut ids = Vec::new();
        while let Some(result) = starts.join_next().await {
            ids.push(result.unwrap().unwrap().id);
        }
        assert!(
            service
                .start("other", "fixture", target.clone())
                .await
                .is_err()
        );
        let mut replacement = target.clone();
        replacement.key = Key::new("account-0", "credential");
        let replacement = service.start("user", "fixture", replacement).await.unwrap();
        assert!(
            service
                .start("other", "fixture", target.clone())
                .await
                .is_err()
        );
        service.cancel("user", replacement.id).await.unwrap();
        service.start("other", "fixture", target).await.unwrap();
        let mut finishes = tokio::task::JoinSet::new();
        for id in ids {
            let service = service.clone();
            finishes.spawn(async move { service.cancel("user", id).await });
        }
        while let Some(result) = finishes.join_next().await {
            result.unwrap().unwrap();
        }
        for slot in service.attempts.lock().await.values() {
            let attempt = slot.lock().await;
            if attempt.status != LoginStatus::Pending {
                assert!(attempt.target.is_none());
                assert!(attempt.session.is_none());
                assert!(attempt.baseline.is_none());
            }
        }
    }

    #[tokio::test]
    async fn login_persists_directly_and_replacement_preserves_identity() {
        let (service, target) = fixture().await;
        let attempt = service
            .start("user", "fixture", target.clone())
            .await
            .unwrap();
        assert!(target.vault.list().await.unwrap().is_empty());
        assert!(
            service
                .advance("other", attempt.id, Some("private"))
                .await
                .is_err()
        );
        assert!(matches!(
            service
                .advance("user", attempt.id, None)
                .await
                .unwrap()
                .status,
            LoginStatus::Pending
        ));
        let done = service
            .advance("user", attempt.id, Some("private"))
            .await
            .unwrap();
        assert!(matches!(done.status, LoginStatus::Validated));
        let id = done.credential_id.unwrap();
        assert!(!serde_json::to_string(&done).unwrap().contains("private"));
        assert!(
            service
                .advance("user", attempt.id, Some("replay"))
                .await
                .is_err()
        );
        assert_eq!(
            service
                .advance("user", attempt.id, None)
                .await
                .unwrap()
                .credential_id,
            Some(id)
        );
        let old = target.vault.status_by_id(id).await.unwrap().unwrap();
        let attempt = service
            .start(
                "user",
                "fixture",
                LoginTarget {
                    expected_id: Some(id),
                    ..target.clone()
                },
            )
            .await
            .unwrap();
        let done = service
            .advance("user", attempt.id, Some("new"))
            .await
            .unwrap();
        assert_eq!(done.credential_id, Some(id));
        assert_ne!(
            target
                .vault
                .status_by_id(id)
                .await
                .unwrap()
                .unwrap()
                .version,
            old.version
        );
        let resolver = ManagedResolver::new(registered());
        let (_, secret) = resolver
            .resolve(&target.vault, id, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(secret.to_env().unwrap()["API_KEY"], "new");
    }

    #[tokio::test]
    async fn cancel_expiry_and_restart_discard_only_attempts() {
        let (service, target) = fixture().await;
        let attempt = service
            .start("user", "fixture", target.clone())
            .await
            .unwrap();
        service.cancel("user", attempt.id).await.unwrap();
        assert!(
            service
                .advance("user", attempt.id, Some("secret"))
                .await
                .is_err()
        );
        let attempt = service
            .start("user", "fixture", target.clone())
            .await
            .unwrap();
        service
            .attempt("user", attempt.id)
            .await
            .unwrap()
            .lock()
            .await
            .expires_at = Utc::now();
        assert!(matches!(
            service
                .advance("user", attempt.id, None)
                .await
                .unwrap()
                .status,
            LoginStatus::Expired
        ));
        let restart =
            ManagedLoginService::new([Arc::new(Provider) as Arc<dyn LoginProvider>]).unwrap();
        assert!(
            restart
                .advance("user", attempt.id, Some("secret"))
                .await
                .is_err()
        );
        assert!(target.vault.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_login_cannot_replace_updated_or_deleted_credentials() {
        for delete in [false, true] {
            let (service, target) = fixture().await;
            let credential = target
                .vault
                .create("static", json!({}), json!({"API_KEY":"old"}))
                .await
                .unwrap();
            let attempt = service
                .start(
                    "user",
                    "fixture",
                    LoginTarget {
                        expected_id: Some(credential.item_id),
                        ..target.clone()
                    },
                )
                .await
                .unwrap();
            if delete {
                target
                    .vault
                    .delete_by_id(credential.item_id, credential.version)
                    .await
                    .unwrap();
            } else {
                target
                    .vault
                    .replace_by_id(
                        credential.item_id,
                        credential.version,
                        "static",
                        json!({}),
                        json!({"API_KEY":"newer"}),
                    )
                    .await
                    .unwrap();
            }
            let done = service
                .advance("user", attempt.id, Some("stale"))
                .await
                .unwrap();
            assert!(matches!(done.status, LoginStatus::Failed));
            assert!(done.credential_id.is_none());
        }
    }

    #[tokio::test]
    async fn concurrent_completion_persists_once_and_scope_checks_are_enforced() {
        let (service, target) = fixture().await;
        let first = service
            .start("user", "fixture", target.clone())
            .await
            .unwrap();
        let second = service
            .start("user", "fixture", target.clone())
            .await
            .unwrap();
        assert!(
            service
                .advance("user", first.id, Some("superseded"))
                .await
                .is_err()
        );
        let other = "other-vault";
        assert!(
            service
                .check_vault("user", second.id, &other)
                .await
                .is_err()
        );
        assert!(
            service
                .check_connection("user", second.id, "other")
                .await
                .is_err()
        );
        let (a, b) = tokio::join!(
            service.advance("user", second.id, Some("one")),
            service.advance("user", second.id, Some("two")),
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(target.vault.list().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn a_deleted_connection_cannot_complete_a_pending_login() {
        let (service, target) = fixture().await;
        let attempt = service
            .start("user", "fixture", target.clone())
            .await
            .unwrap();
        target.vault.delete_connection("alice").await.unwrap();
        assert!(
            service
                .advance("user", attempt.id, Some("approved"))
                .await
                .is_err()
        );
        assert!(target.vault.ensure_available().await.is_err());
    }
}
