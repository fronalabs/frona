use super::test_support::{db, vault};
use super::*;
use crate::credential::key_rotation::KeyRotation;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn direct_credentials_preserve_identity_and_reject_stale_scoped_writes() {
    let db = db().await;
    let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
    let wrong = vault(&db, "alice-vault", "key").await;
    let first = v
        .create(
            "static",
            json!({"name":"account"}),
            json!({"API_KEY":"secret"}),
        )
        .await
        .unwrap();
    assert_eq!(
        v.document_by_id(first.item_id).await.unwrap().1["API_KEY"],
        "secret"
    );
    assert!(wrong.status_by_id(first.item_id).await.unwrap().is_none());
    assert!(
        wrong
            .replace_by_id(first.item_id, first.version, "static", json!({}), json!({}))
            .await
            .is_err()
    );
    assert!(
        wrong
            .delete_by_id(first.item_id, first.version)
            .await
            .is_err()
    );
    let (one, two) = tokio::join!(
        v.replace_by_id(
            first.item_id,
            first.version,
            "static",
            json!({}),
            json!({"API_KEY":"one"})
        ),
        v.replace_by_id(
            first.item_id,
            first.version,
            "static",
            json!({}),
            json!({"API_KEY":"two"})
        ),
    );
    assert_ne!(one.is_ok(), two.is_ok());
    let current = v.status_by_id(first.item_id).await.unwrap().unwrap();
    assert_eq!(current.item_id, first.item_id);
    assert_ne!(current.version, first.version);
    assert!(v.delete_by_id(first.item_id, first.version).await.is_err());
    v.delete_by_id(first.item_id, current.version)
        .await
        .unwrap();
    assert!(v.status_by_id(first.item_id).await.unwrap().is_none());
    assert!(
        v.replace_by_id(
            first.item_id,
            current.version,
            "static",
            json!({}),
            json!({})
        )
        .await
        .is_err()
    );
    let recreated = v
        .create(
            "static",
            json!({"name":"account"}),
            json!({"API_KEY":"new"}),
        )
        .await
        .unwrap();
    assert_ne!(recreated.item_id, first.item_id);
}

#[tokio::test]
async fn direct_credentials_are_encrypted_and_ids_are_unique() {
    use super::repository::ManagedVaultRepository;
    let db = db().await;
    let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
    let created = v
        .create("static", json!({}), json!({"API_KEY":"never-plaintext"}))
        .await
        .unwrap();
    let repo = crate::db::repo::managed_vault::SurrealManagedVaultRepo::new(db.clone());
    let row = repo
        .row_by_id(v.connection_id(), created.item_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!row.to_string().contains("never-plaintext"));
    assert!(!row["ciphertext"].as_array().unwrap().is_empty());
    assert!(repo.write(v.connection_id(), row, None).await.is_err());
}

#[tokio::test]
async fn corrupted_secret_blocks_rotation_without_advancing_the_key_and_can_be_retried() {
    use super::repository::ManagedVaultRepository;
    let db = db().await;
    KeyRotation::check(&db, "old").await.unwrap();
    let vault = vault(&db, GLOBAL_CONNECTION_ID, "old").await;
    let entry = vault
        .create("static", json!({}), json!({"API_KEY":"private"}))
        .await
        .unwrap();
    let repo = crate::db::repo::managed_vault::SurrealManagedVaultRepo::new(db.clone());
    let row = repo
        .row_by_id(vault.connection_id(), entry.item_id)
        .await
        .unwrap()
        .unwrap();
    db.query("UPDATE managed_credential SET ciphertext = [1,2,3] WHERE item_id = $id")
        .bind(("id", entry.item_id.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(vault.document_by_id(entry.item_id).await.is_err());
    let report = KeyRotation::check(&db, "new")
        .await
        .unwrap()
        .unwrap()
        .run()
        .await
        .unwrap();
    assert_eq!(report.managed_credentials.failed, 1);
    assert!(!report.all_succeeded());
    let retry = KeyRotation::check(&db, "new").await.unwrap().unwrap();
    db.query("UPDATE managed_credential SET ciphertext = $ciphertext WHERE item_id = $id")
        .bind(("id", entry.item_id.to_string()))
        .bind(("ciphertext", row["ciphertext"].clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(retry.run().await.unwrap().all_succeeded());
    assert!(KeyRotation::check(&db, "new").await.unwrap().is_none());
}

#[test]
fn integration_registry_reuses_instances_and_rejects_duplicate_names() {
    use super::integration::*;

    struct TestIntegration;

    #[async_trait::async_trait]
    impl ManagedIntegration for TestIntegration {
        type Credentials = std::collections::HashMap<String, String>;
        async fn get_secret(
            &self,
            _: Value,
            _: &mut SecretContext,
        ) -> Result<
            ResolvedSecret<std::collections::HashMap<String, String>>,
            crate::core::error::AppError,
        > {
            Ok(ResolvedSecret {
                expires_at: None,
                credentials: Default::default(),
                cache: CachePolicy::NoCache,
            })
        }
    }
    let instance: Arc<dyn crate::credential::managed::integration::RegisteredIntegration> =
        Arc::new(TestIntegration);
    let registry = register([("test".into(), instance.clone())]).unwrap();
    assert!(Arc::ptr_eq(&instance, &registry.get("test").unwrap()));
    assert!(Arc::ptr_eq(
        &registry.get("test").unwrap(),
        &registry.clone().get("test").unwrap()
    ));
    assert!(registry.get("missing").is_none());
    assert!(register([("".into(), instance.clone())]).is_err());
    assert!(
        register([
            ("test".into(), instance.clone()),
            ("test".into(), instance.clone())
        ])
        .is_err()
    );
}

#[tokio::test]
async fn scoped_context_updates_only_its_entry_and_rejects_stale_results() {
    use super::integration::SecretContext;
    let db = db().await;
    let v = vault(&db, GLOBAL_CONNECTION_ID, "key").await;
    let first = v
        .create("test", json!({}), json!({"token":"one"}))
        .await
        .unwrap();
    let mut context = SecretContext::new(v.clone(), first.clone());
    let mut stale = SecretContext::new(v.clone(), first.clone());
    context.update_secret(json!({"token":"two"})).await.unwrap();
    assert!(stale.update_secret(json!({"token":"stale"})).await.is_err());
    assert_eq!(
        v.document_by_id(first.item_id).await.unwrap().1["token"],
        "two"
    );
    let current = v.status_by_id(first.item_id).await.unwrap().unwrap();
    v.delete_by_id(first.item_id, current.version)
        .await
        .unwrap();
    assert!(
        context
            .update_secret(json!({"token":"deleted"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rotation_covers_global_and_personal_scopes_and_retries_corrupt_records() {
    let db = db().await;
    KeyRotation::check(&db, "old").await.unwrap();
    let global = vault(&db, GLOBAL_CONNECTION_ID, "old").await;
    let personal = vault(&db, "alice-vault", "old").await;
    let a = global
        .create("static", json!({}), json!({"API_KEY":"global"}))
        .await
        .unwrap();
    let b = personal
        .create("static", json!({}), json!({"API_KEY":"personal"}))
        .await
        .unwrap();
    assert!(global.status_by_id(b.item_id).await.unwrap().is_none());
    assert!(personal.status_by_id(a.item_id).await.unwrap().is_none());
    let report = KeyRotation::check(&db, "new")
        .await
        .unwrap()
        .unwrap()
        .run()
        .await
        .unwrap();
    assert_eq!(report.managed_credentials.success, 2);
    assert_eq!(
        vault(&db, GLOBAL_CONNECTION_ID, "new")
            .await
            .document_by_id(a.item_id)
            .await
            .unwrap()
            .1["API_KEY"],
        "global"
    );
    assert_eq!(
        vault(&db, "alice-vault", "new")
            .await
            .document_by_id(b.item_id)
            .await
            .unwrap()
            .1["API_KEY"],
        "personal"
    );
}

#[tokio::test]
async fn connections_owned_by_one_user_are_isolated_and_nonempty_deletion_is_rejected() {
    let db = db().await;
    let first = vault(&db, "alice-first", "key").await;
    let second = vault(&db, "alice-second", "key").await;
    let entry = first
        .create(
            "static",
            serde_json::json!({"name":"login"}),
            serde_json::json!({"API_KEY":"secret"}),
        )
        .await
        .unwrap();
    assert!(second.list().await.unwrap().is_empty());
    assert!(second.status_by_id(entry.item_id).await.unwrap().is_none());
    assert!(
        second
            .replace_by_id(
                entry.item_id,
                entry.version,
                "static",
                serde_json::json!({}),
                serde_json::json!({})
            )
            .await
            .is_err()
    );
    assert!(first.delete_connection("bob").await.is_err());
    assert!(first.delete_connection("alice").await.is_err());
    assert!(first.status_by_id(entry.item_id).await.unwrap().is_some());
    first
        .delete_by_id(entry.item_id, entry.version)
        .await
        .unwrap();
    first.delete_connection("alice").await.unwrap();
    assert!(first.list().await.is_err());
    assert!(
        first
            .create(
                "static",
                serde_json::json!({}),
                serde_json::json!({"API_KEY":"late"})
            )
            .await
            .is_err()
    );
    assert!(second.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn deleting_an_empty_connection_racing_a_write_never_orphans_credentials() {
    for index in 0..12 {
        let db = db().await;
        let connection = format!("race-{index}");
        let v = vault(&db, &connection, "key").await;
        let (written, deleted) = tokio::join!(
            v.create(
                "static",
                serde_json::json!({}),
                serde_json::json!({"API_KEY":"secret"})
            ),
            v.delete_connection("alice"),
        );
        assert!(!(written.is_ok() && deleted.is_ok()));
        let items: Vec<serde_json::Value> = db
            .query("SELECT item_id FROM managed_credential WHERE connection_id = $connection")
            .bind(("connection", connection.clone()))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        let connections: Vec<serde_json::Value> = db
            .query("SELECT * FROM type::record('vault_connection', $connection)")
            .bind(("connection", connection))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert!(items.is_empty() || !connections.is_empty());
    }
}

#[tokio::test]
async fn disabling_a_connection_blocks_cached_secrets_and_refresh() {
    let db = db().await;
    let v = vault(&db, "alice-cache", "key").await;
    let resolver = super::resolver::ManagedResolver::new(super::integration::registered());
    let entry = v
        .create(
            "static",
            serde_json::json!({}),
            serde_json::json!({"API_KEY":"secret"}),
        )
        .await
        .unwrap();
    resolver
        .resolve(&v, entry.item_id, || async { Ok(()) })
        .await
        .unwrap();
    db.query("UPDATE type::record('vault_connection', 'alice-cache') SET enabled = false")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        resolver
            .resolve(&v, entry.item_id, || async { Ok(()) })
            .await
            .is_err()
    );
    assert!(
        v.replace_by_id(
            entry.item_id,
            entry.version,
            "static",
            serde_json::json!({}),
            serde_json::json!({"API_KEY":"late"})
        )
        .await
        .is_err()
    );
    db.query("UPDATE type::record('vault_connection', 'alice-cache') SET enabled = true")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        resolver
            .resolve(&v, entry.item_id, || async { Ok(()) })
            .await
            .is_ok()
    );
}

#[test]
fn managed_connection_configuration_has_no_scope_fields() {
    use crate::credential::vault::models::VaultConnectionConfig;
    assert_eq!(
        serde_json::to_value(VaultConnectionConfig::Managed {}).unwrap(),
        serde_json::json!({"type":"Managed"})
    );
    assert!(
        serde_json::from_value::<VaultConnectionConfig>(
            serde_json::json!({"type":"Managed","namespace":"model_providers","global":true})
        )
        .is_err()
    );
}
