use super::{
    ManagedVault, Status,
    integration::{CachePolicy, SecretContext},
};
use crate::core::error::AppError;
use chrono::{DateTime, Utc};
use std::{collections::HashMap, future::Future, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

type Entry = Arc<Mutex<Option<Cached>>>;
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
const MAX_IDLE_ENTRIES: usize = 1024;

struct Cached {
    version: Uuid,
    generation: u64,
    secret: super::integration::ErasedSecret,
}

/// Shared by trusted inference and permission-checked vault consumers.
/// The caller supplies authorization internally; no API accepts a bypass flag.
pub struct ManagedResolver {
    integrations:
        HashMap<String, Arc<dyn crate::credential::managed::integration::RegisteredIntegration>>,
    entries: Mutex<HashMap<Uuid, Entry>>,
    now: Clock,
}

impl ManagedResolver {
    pub fn new(
        integrations: HashMap<
            String,
            Arc<dyn crate::credential::managed::integration::RegisteredIntegration>,
        >,
    ) -> Self {
        Self {
            integrations,
            entries: Mutex::new(HashMap::new()),
            now: Arc::new(Utc::now),
        }
    }

    pub(crate) async fn resolve<F, Fut>(
        &self,
        vault: &ManagedVault,
        id: Uuid,
        authorize: F,
    ) -> Result<(Status, super::integration::ErasedSecret), AppError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), AppError>>,
    {
        authorize().await?;
        let entry = {
            let mut entries = self.entries.lock().await;
            if entries.len() >= MAX_IDLE_ENTRIES {
                // Only evict idle entries. Waiting/in-flight requests retain the
                // same lock, so eviction cannot start competing resolutions.
                entries.retain(|_, entry| Arc::strong_count(entry) > 1);
            }
            entries
                .entry(id)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        let mut cached = entry.lock().await;
        authorize().await?;
        let status = vault
            .status_by_id(id)
            .await?
            .filter(|status| !status.removed)
            .ok_or_else(|| AppError::NotFound("active managed credential".into()))?;
        self.integrations
            .get(&status.integration)
            .ok_or_else(|| AppError::Validation("managed integration is unavailable".into()))?;
        if let Some(hit) = cached.as_ref().filter(|hit| {
            hit.version == status.version
                && hit.generation == status.generation
                && self.valid(hit.secret.cache)
                && hit
                    .secret
                    .expires_at
                    .is_none_or(|expiry| expiry > (self.now)())
        }) {
            let ctx = SecretContext::new(vault.clone(), status.clone());
            ctx.verify().await?;
            authorize().await?;
            if self.valid(hit.secret.cache)
                && hit
                    .secret
                    .expires_at
                    .is_none_or(|expiry| expiry > (self.now)())
            {
                return Ok((status, hit.secret.clone()));
            }
        }
        *cached = None;
        let (status, doc) = vault.document_by_id(id).await?;
        // Resolve the integration from this exact document snapshot, not a
        // potentially superseded earlier status read.
        let integration = self
            .integrations
            .get(&status.integration)
            .ok_or_else(|| AppError::Validation("managed integration is unavailable".into()))?;
        let mut ctx = SecretContext::new(vault.clone(), status);
        let secret = integration
            .resolve_erased(doc, &mut ctx)
            .await
            .map_err(|_| {
                AppError::Validation(format!("managed credential {id} could not be resolved"))
            })?;
        ctx.verify().await?;
        authorize().await?;
        if matches!(secret.cache, CachePolicy::Until(expiry) if expiry <= (self.now)())
            || secret
                .expires_at
                .is_some_and(|expiry| expiry <= (self.now)())
        {
            return Err(AppError::Validation(format!(
                "managed credential {id} returned an expired secret"
            )));
        }
        if secret.cache != CachePolicy::NoCache {
            *cached = Some(Cached {
                version: ctx.version(),
                generation: ctx.status().generation,
                secret: secret.clone(),
            });
        }
        Ok((ctx.status().clone(), secret))
    }

    fn valid(&self, policy: CachePolicy) -> bool {
        match policy {
            CachePolicy::NoCache => false,
            CachePolicy::Until(expiry) => expiry > (self.now)(),
            CachePolicy::UntilChanged => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::managed::integration::register;
    use crate::credential::managed::{
        GLOBAL_CONNECTION_ID,
        integration::{ManagedIntegration, ResolvedSecret},
        test_support::{db, vault},
    };
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use tokio::sync::{Barrier, Notify};

    struct Integration {
        calls: AtomicUsize,
        fail: AtomicBool,
        policy: CachePolicy,
        expiry: Option<DateTime<Utc>>,
        update: bool,
        started: Option<Arc<Notify>>,
        release: Option<Arc<Notify>>,
        barrier: Option<Arc<Barrier>>,
    }

    impl Integration {
        fn new(policy: CachePolicy) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
                policy,
                expiry: None,
                update: false,
                started: None,
                release: None,
                barrier: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl ManagedIntegration for Integration {
        type Credentials = std::collections::HashMap<String, String>;
        async fn get_secret(
            &self,
            doc: Value,
            ctx: &mut SecretContext,
        ) -> Result<ResolvedSecret<std::collections::HashMap<String, String>>, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(AppError::Validation("private-refresh-secret".into()));
            }
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            if self.update {
                ctx.update_secret(json!({"token":"updated", "private":"renewable"}))
                    .await?;
            }
            Ok(ResolvedSecret {
                expires_at: self.expiry,
                credentials: [
                    (
                        "TOKEN".into(),
                        if self.update {
                            "updated".into()
                        } else {
                            doc["token"].as_str().unwrap_or("fixture").into()
                        },
                    ),
                    ("ACCOUNT".into(), ctx.credential_id().to_string()),
                ]
                .into(),
                cache: self.policy,
            })
        }
    }

    fn resolver(integration: Arc<Integration>, clock: Arc<AtomicI64>) -> Arc<ManagedResolver> {
        let mut resolver = ManagedResolver::new(
            register([(
                "test".into(),
                integration
                    as Arc<dyn crate::credential::managed::integration::RegisteredIntegration>,
            )])
            .unwrap(),
        );
        resolver.now =
            Arc::new(move || DateTime::from_timestamp(clock.load(Ordering::SeqCst), 0).unwrap());
        Arc::new(resolver)
    }

    #[tokio::test]
    async fn hard_expiry_overrides_until_changed_and_resolution_identity_tracks_refresh() {
        let db = db().await;
        let vault = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
        let status = entry(&vault, "one").await;
        let clock = Arc::new(AtomicI64::new(100));
        let mut integration = Integration::new(CachePolicy::UntilChanged);
        integration.expiry = DateTime::from_timestamp(200, 0);
        let integration = Arc::new(integration);
        let resolver = resolver(integration.clone(), clock.clone());
        let first = resolver
            .resolve(&vault, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        let cached = resolver
            .resolve(&vault, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(first.1.identity, cached.1.identity);
        clock.store(200, Ordering::SeqCst);
        assert!(
            resolver
                .resolve(&vault, status.item_id, || async { Ok(()) })
                .await
                .is_err()
        );
        assert_eq!(integration.calls.load(Ordering::SeqCst), 2);
        clock.store(100, Ordering::SeqCst);
        let refreshed = resolver
            .resolve(&vault, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(first.0.version, refreshed.0.version);
        assert_ne!(first.1.identity, refreshed.1.identity);
    }

    #[tokio::test]
    async fn missing_integration_returns_validation_error() {
        let db = db().await;
        let vault = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
        let status = entry(&vault, "one").await;
        let resolver = ManagedResolver::new(HashMap::new());

        let result = resolver
            .resolve(&vault, status.item_id, || async { Ok(()) })
            .await;
        assert!(
            matches!(result, Err(AppError::Validation(message)) if message == "managed integration is unavailable")
        );
    }

    #[tokio::test]
    async fn consumer_startup_obeys_cache_expiry_and_binding_removal() {
        use crate::core::{Principal, repository::Repository};
        use crate::credential::vault::{models::*, service::VaultService};
        use crate::db::repo::generic::SurrealRepo;
        for policy in [
            CachePolicy::NoCache,
            CachePolicy::UntilChanged,
            CachePolicy::Until(DateTime::from_timestamp(200, 0).unwrap()),
        ] {
            let db = db().await;
            let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
            let status = entry(&v, "one").await;
            let clock = Arc::new(AtomicI64::new(100));
            let integration = Arc::new(Integration::new(policy));
            let r = resolver(integration.clone(), clock.clone());
            let config = crate::core::config::Config::default();
            let svc = VaultService::new(
                Arc::new(SurrealRepo::new(db.clone())),
                Arc::new(SurrealRepo::new(db.clone())),
                Arc::new(SurrealRepo::new(db.clone())),
                Arc::new(SurrealRepo::new(db.clone())),
                Arc::new(SurrealRepo::new(db.clone())),
                "key",
                Default::default(),
                "/tmp/test-data".into(),
                crate::storage::StorageService::new(&config),
                crate::auth::UserService::new(SurrealRepo::new(db.clone()), &Default::default()),
                v.clone(),
                r,
                crate::credential::managed::login::ManagedLoginService::registered(),
            );
            svc.sync_config_connections().await.unwrap();
            let principal = Principal::agent("agent");
            let mut grant = svc
                .create_grant(
                    "user",
                    principal.clone(),
                    "managed",
                    &status.item_id.to_string(),
                    "login",
                    &GrantDuration::Permanent,
                )
                .await
                .unwrap();
            svc.create_binding(
                "user",
                principal.clone(),
                "login",
                "managed",
                &status.item_id.to_string(),
                CredentialTarget::Prefix {
                    env_var_prefix: "LOGIN".into(),
                },
                BindingScope::Durable,
                None,
            )
            .await
            .unwrap();
            for _ in 0..2 {
                let env = svc.resolve_env("user", &principal, None).await.unwrap();
                assert_eq!(env.len(), 2);
                assert!(
                    env.iter()
                        .any(|(k, v)| k == "LOGIN_TOKEN" && v == "synthetic-secret")
                );
            }
            assert_eq!(
                integration.calls.load(Ordering::SeqCst),
                if policy == CachePolicy::NoCache { 2 } else { 1 }
            );
            if matches!(policy, CachePolicy::Until(_)) {
                clock.store(200, Ordering::SeqCst);
                let error = svc.resolve_env("user", &principal, None).await.unwrap_err();
                assert!(!error.to_string().contains("private-refresh"));
                assert_eq!(integration.calls.load(Ordering::SeqCst), 2);
                clock.store(100, Ordering::SeqCst);
            }
            // A cached map cannot extend an expired grant.
            grant.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
            SurrealRepo::<VaultGrant>::new(db.clone())
                .update(&grant)
                .await
                .unwrap();
            assert!(svc.resolve_env("user", &principal, None).await.is_err());
            svc.delete_bindings_for_principal("user", &principal)
                .await
                .unwrap();
            assert!(
                svc.resolve_env("user", &principal, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    async fn entry(v: &ManagedVault, name: &str) -> Status {
        v.create(
            "test",
            json!({"name":name}),
            json!({"token":"synthetic-secret"}),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn all_cache_policies_and_expiry_are_enforced() {
        let db = db().await;
        let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
        let status = entry(&v, "one").await;
        for policy in [
            CachePolicy::NoCache,
            CachePolicy::UntilChanged,
            CachePolicy::Until(DateTime::from_timestamp(200, 0).unwrap()),
        ] {
            let clock = Arc::new(AtomicI64::new(100));
            let integration = Arc::new(Integration::new(policy));
            let r = resolver(integration.clone(), clock.clone());
            for _ in 0..2 {
                let (_, secret) = r
                    .resolve(&v, status.item_id, || async { Ok(()) })
                    .await
                    .unwrap();
                assert_eq!(secret.to_env().unwrap().len(), 2);
                assert!(!format!("{secret:?}").contains("synthetic-secret"));
            }
            assert_eq!(
                integration.calls.load(Ordering::SeqCst),
                if policy == CachePolicy::NoCache { 2 } else { 1 }
            );
            if matches!(policy, CachePolicy::Until(_)) {
                clock.store(200, Ordering::SeqCst);
                assert!(
                    r.resolve(&v, status.item_id, || async { Ok(()) })
                        .await
                        .is_err()
                );
                assert_eq!(integration.calls.load(Ordering::SeqCst), 2);
            }
            r.entries.lock().await.clear();
            clock.store(100, Ordering::SeqCst);
            r.resolve(&v, status.item_id, || async { Ok(()) })
                .await
                .unwrap();
            assert!(integration.calls.load(Ordering::SeqCst) >= 2);
        }
    }

    #[tokio::test]
    async fn context_update_caches_the_new_version_and_updates_invalidate() {
        let db = db().await;
        let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
        let status = entry(&v, "one").await;
        let mut integration = Integration::new(CachePolicy::UntilChanged);
        integration.update = true;
        let integration = Arc::new(integration);
        let r = resolver(integration.clone(), Arc::new(AtomicI64::new(100)));
        let (updated, secret) = r
            .resolve(&v, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        assert_ne!(updated.version, status.version);
        assert_eq!(updated.item_id, status.item_id);
        assert!(!secret.to_env().unwrap().contains_key("private"));
        assert_eq!(
            v.document_by_id(status.item_id).await.unwrap().1["private"],
            "renewable"
        );
        r.resolve(&v, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(integration.calls.load(Ordering::SeqCst), 1);
        v.replace_by_id(
            updated.item_id,
            updated.version,
            "test",
            json!({}),
            json!({"token":"replacement"}),
        )
        .await
        .unwrap();
        r.resolve(&v, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(integration.calls.load(Ordering::SeqCst), 2);
        let current = v.status_by_id(status.item_id).await.unwrap().unwrap();
        v.delete_by_id(current.item_id, current.version)
            .await
            .unwrap();
        assert!(
            r.resolve(&v, status.item_id, || async { Ok(()) })
                .await
                .is_err()
        );
        let fresh = entry(&v, "one").await;
        assert_ne!(fresh.item_id, status.item_id);
        r.resolve(&v, fresh.item_id, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(integration.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn cached_values_do_not_bypass_authorization_or_scope() {
        let db = db().await;
        let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
        let status = entry(&v, "one").await;
        let integration = Arc::new(Integration::new(CachePolicy::UntilChanged));
        let r = resolver(integration.clone(), Arc::new(AtomicI64::new(100)));
        r.resolve(&v, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        assert!(
            r.resolve(&v, status.item_id, || async {
                Err(AppError::Forbidden("revoked".into()))
            })
            .await
            .is_err()
        );
        let other = vault(&db, "alice-vault", "key").await;
        assert!(
            r.resolve(&other, status.item_id, || async { Ok(()) })
                .await
                .is_err()
        );
        assert_eq!(integration.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_failure_never_returns_stale_values_or_private_errors() {
        let db = db().await;
        let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
        let status = entry(&v, "one").await;
        let clock = Arc::new(AtomicI64::new(100));
        let integration = Arc::new(Integration::new(CachePolicy::Until(
            DateTime::from_timestamp(200, 0).unwrap(),
        )));
        let r = resolver(integration.clone(), clock.clone());
        r.resolve(&v, status.item_id, || async { Ok(()) })
            .await
            .unwrap();
        clock.store(200, Ordering::SeqCst);
        integration.fail.store(true, Ordering::SeqCst);
        let error = r
            .resolve(&v, status.item_id, || async { Ok(()) })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(&status.item_id.to_string()));
        assert!(!error.contains("private-refresh-secret"));
        assert!(
            r.entries.lock().await[&status.item_id]
                .lock()
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn same_credential_requests_share_resolution_but_different_entries_do_not_block() {
        let db = db().await;
        let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
        let a = entry(&v, "a").await;
        let b = entry(&v, "b").await;
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut integration = Integration::new(CachePolicy::UntilChanged);
        integration.started = Some(started.clone());
        integration.release = Some(release.clone());
        let integration = Arc::new(integration);
        let r = resolver(integration.clone(), Arc::new(AtomicI64::new(100)));
        let task = {
            let r = r.clone();
            let v = v.clone();
            tokio::spawn(async move { r.resolve(&v, a.item_id, || async { Ok(()) }).await })
        };
        started.notified().await;
        let second = r.resolve(&v, a.item_id, || async { Ok(()) });
        tokio::pin!(second);
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(integration.calls.load(Ordering::SeqCst), 1);
        release.notify_one();
        task.await.unwrap().unwrap();
        second.await.unwrap();
        assert_eq!(integration.calls.load(Ordering::SeqCst), 1);
        let mut parallel = Integration::new(CachePolicy::NoCache);
        parallel.barrier = Some(Arc::new(Barrier::new(2)));
        let r = resolver(Arc::new(parallel), Arc::new(AtomicI64::new(100)));
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                r.resolve(&v, a.item_id, || async { Ok(()) }),
                r.resolve(&v, b.item_id, || async { Ok(()) })
            )
        })
        .await
        .unwrap();
        assert_ne!(
            a.unwrap().1.to_env().unwrap()["ACCOUNT"],
            b.unwrap().1.to_env().unwrap()["ACCOUNT"]
        );
    }

    #[tokio::test]
    async fn in_flight_results_are_rejected_after_revoke_update_or_delete() {
        for mutation in ["revoke", "update", "delete"] {
            let db = db().await;
            let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
            let status = entry(&v, "one").await;
            let allowed = Arc::new(AtomicBool::new(true));
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let mut integration = Integration::new(CachePolicy::UntilChanged);
            integration.started = Some(started.clone());
            integration.release = Some(release.clone());
            let r = resolver(Arc::new(integration), Arc::new(AtomicI64::new(100)));
            let task = {
                let v = v.clone();
                let r = r.clone();
                let allowed = allowed.clone();
                tokio::spawn(async move {
                    r.resolve(&v, status.item_id, || async {
                        if allowed.load(Ordering::SeqCst) {
                            Ok(())
                        } else {
                            Err(AppError::Forbidden("revoked".into()))
                        }
                    })
                    .await
                })
            };
            started.notified().await;
            match mutation {
                "revoke" => allowed.store(false, Ordering::SeqCst),
                "update" => {
                    v.replace_by_id(
                        status.item_id,
                        status.version,
                        "test",
                        json!({}),
                        json!({"token":"new"}),
                    )
                    .await
                    .unwrap();
                }
                _ => {
                    let current = v.status_by_id(status.item_id).await.unwrap().unwrap();
                    v.delete_by_id(current.item_id, current.version)
                        .await
                        .unwrap();
                }
            }
            release.notify_one();
            assert!(task.await.unwrap().is_err());
        }
    }
}
