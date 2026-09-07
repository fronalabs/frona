use super::*;
use crate::credential::managed::{ManagedVault, resolver::ManagedResolver};
use serde_json::json;

async fn vault() -> ManagedVault {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    crate::credential::managed::test_support::vault(&db, "credentials", "fixture-secret").await
}

#[tokio::test]
async fn static_composite_documents_invalidate_on_update_and_authorization_runs_on_cache_hits() {
    let vault = vault().await;
    let resolver = ManagedResolver::new(registered());
    let first = vault
        .create(
            "static",
            json!({}),
            json!({"USERNAME":"one","PASSWORD":"secret"}),
        )
        .await
        .unwrap();
    let (_, resolved) = resolver
        .resolve(&vault, first.item_id, || async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(resolved.to_env().unwrap().len(), 2);
    assert_eq!(resolved.cache, CachePolicy::UntilChanged);
    let typed = resolved.credentials::<static_secret::Document>().unwrap();
    assert_eq!(typed["USERNAME"], "one");
    assert!(
        resolved
            .credentials::<rig_core::providers::chatgpt::ChatGPTAuth>()
            .is_err()
    );
    let (_, cached) = resolver
        .resolve(&vault, first.item_id, || async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(resolved.identity, cached.identity);
    assert!(std::ptr::eq(
        typed,
        cached.credentials::<static_secret::Document>().unwrap()
    ));
    assert!(
        resolver
            .resolve(&vault, first.item_id, || async {
                Err(crate::core::error::AppError::Forbidden("revoked".into()))
            })
            .await
            .is_err()
    );
    let second = vault
        .replace_by_id(
            first.item_id,
            first.version,
            "static",
            json!({}),
            json!({"USERNAME":"two","PASSWORD":"new"}),
        )
        .await
        .unwrap();
    assert_eq!(first.item_id, second.item_id);
    assert_eq!(
        resolver
            .resolve(&vault, first.item_id, || async { Ok(()) })
            .await
            .unwrap()
            .1
            .to_env()
            .unwrap()["USERNAME"],
        "two"
    );
}

#[test]
fn resolved_export_errors_do_not_expose_integration_details() {
    struct FailingExport;

    impl SecretEnv for FailingExport {
        fn to_env(&self) -> Result<HashMap<String, String>, crate::core::error::AppError> {
            Err(crate::core::error::AppError::Validation(
                "private-refresh-token".into(),
            ))
        }
    }

    let secret = ResolvedSecret {
        credentials: FailingExport,
        expires_at: None,
        cache: CachePolicy::NoCache,
    };
    assert!(
        !secret
            .to_env()
            .unwrap_err()
            .to_string()
            .contains("private-refresh-token")
    );
    assert!(
        !secret
            .erase()
            .to_env()
            .unwrap_err()
            .to_string()
            .contains("private-refresh-token")
    );
}
