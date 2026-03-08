use frona::api::db::setup_schema;
use frona::api::repo::generic::SurrealRepo;
use frona::credential::vault::models::*;
use frona::credential::vault::repository::{VaultAccessLogRepository, VaultConnectionRepository, VaultGrantRepository};
use frona::credential::vault::service::VaultService;
use frona::core::config::VaultConfig;
use std::sync::Arc;

async fn setup_db() -> surrealdb::Surreal<surrealdb::engine::local::Db> {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    setup_schema(&db).await.unwrap();
    db
}

async fn create_test_connection(svc: &VaultService, user_id: &str) -> VaultConnectionResponse {
    svc.create_connection(
        user_id,
        CreateVaultConnectionRequest {
            name: "test-conn".into(),
            provider: VaultProviderType::Hashicorp,
            config: VaultConnectionConfig::Hashicorp {
                address: "http://localhost:8200".into(),
                token: "tok".into(),
                mount_path: None,
            },
        },
    )
    .await
    .unwrap()
}

fn build_service(db: &surrealdb::Surreal<surrealdb::engine::local::Db>) -> VaultService {
    let connection_repo: Arc<dyn VaultConnectionRepository> =
        Arc::new(SurrealRepo::<VaultConnection>::new(db.clone()));
    let grant_repo: Arc<dyn VaultGrantRepository> =
        Arc::new(SurrealRepo::<VaultGrant>::new(db.clone()));
    let credential_repo: Arc<dyn frona::credential::vault::repository::CredentialRepository> =
        Arc::new(SurrealRepo::<frona::credential::vault::models::Credential>::new(db.clone()));
    let access_log_repo: Arc<dyn VaultAccessLogRepository> =
        Arc::new(SurrealRepo::<VaultAccessLog>::new(db.clone()));
    VaultService::new(
        connection_repo,
        grant_repo,
        credential_repo,
        access_log_repo,
        "test-secret",
        VaultConfig::default(),
    )
}

#[tokio::test]
async fn create_and_list_connections() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let resp = svc
        .create_connection(
            "user1",
            CreateVaultConnectionRequest {
                name: "My Vault".into(),
                provider: VaultProviderType::Hashicorp,
                config: VaultConnectionConfig::Hashicorp {
                    address: "http://localhost:8200".into(),
                    token: "hvs.test".into(),
                    mount_path: None,
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(resp.name, "My Vault");
    assert_eq!(resp.provider, VaultProviderType::Hashicorp);
    assert!(resp.enabled);
    assert!(!resp.system_managed);

    let list = svc.list_connections("user1").await.unwrap();
    // Should have: user-created + local (system-managed)
    assert!(list.len() >= 2);
    assert!(list.iter().any(|c| c.name == "My Vault"));
    assert!(list.iter().any(|c| c.id == "local" && c.system_managed));
}

#[tokio::test]
async fn delete_connection_removes_grants() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let conn = svc
        .create_connection(
            "user1",
            CreateVaultConnectionRequest {
                name: "temp".into(),
                provider: VaultProviderType::Hashicorp,
                config: VaultConnectionConfig::Hashicorp {
                    address: "http://localhost:8200".into(),
                    token: "tok".into(),
                    mount_path: None,
                },
            },
        )
        .await
        .unwrap();

    svc.create_grant(
        "user1",
        "agent1",
        &conn.id,
        "item1",
        "github",
        None,
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();

    let grants_before = svc.list_grants("user1").await.unwrap();
    assert_eq!(grants_before.len(), 1);

    svc.delete_connection("user1", &conn.id).await.unwrap();

    let grants_after = svc.list_grants("user1").await.unwrap();
    assert!(grants_after.is_empty());
}

#[tokio::test]
async fn find_matching_grant_by_query() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    svc.create_grant(
        "user1",
        "agent1",
        &conn.id,
        "item1",
        "github",
        None,
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();

    let found = svc
        .find_matching_grant("user1", "agent1", "github", None)
        .await
        .unwrap();
    assert!(found.is_some());

    let not_found = svc
        .find_matching_grant("user1", "agent1", "gitlab", None)
        .await
        .unwrap();
    assert!(not_found.is_none());
}

#[tokio::test]
async fn find_matching_grant_by_env_var_prefix() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    svc.create_grant(
        "user1",
        "agent1",
        &conn.id,
        "item1",
        "github",
        Some("GH"),
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();

    let found = svc
        .find_matching_grant("user1", "agent1", "unrelated-query", Some("GH"))
        .await
        .unwrap();
    assert!(found.is_some());
}

#[tokio::test]
async fn expired_grant_is_cleaned_up() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    let grant_repo: Arc<dyn VaultGrantRepository> =
        Arc::new(SurrealRepo::<VaultGrant>::new(db.clone()));

    let expired_grant = VaultGrant {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user1".into(),
        connection_id: conn.id,
        vault_item_id: "item1".into(),
        agent_id: "agent1".into(),
        query: "old-service".into(),
        env_var_prefix: None,
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        created_at: chrono::Utc::now(),
    };
    grant_repo.create(&expired_grant).await.unwrap();

    let result = svc
        .find_matching_grant("user1", "agent1", "old-service", None)
        .await
        .unwrap();
    assert!(result.is_none(), "Expired grant should not match");
}

#[tokio::test]
async fn toggle_connection() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let conn = svc
        .create_connection(
            "user1",
            CreateVaultConnectionRequest {
                name: "test".into(),
                provider: VaultProviderType::Hashicorp,
                config: VaultConnectionConfig::Hashicorp {
                    address: "http://localhost:8200".into(),
                    token: "tok".into(),
                    mount_path: None,
                },
            },
        )
        .await
        .unwrap();
    assert!(conn.enabled);

    let toggled = svc.toggle_connection("user1", &conn.id, false).await.unwrap();
    assert!(!toggled.enabled);
}

#[tokio::test]
async fn cannot_delete_system_managed_connection() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let result = svc.delete_connection("user1", "local").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn revoke_grant() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let grant = svc
        .create_grant(
            "user1",
            "agent1",
            "conn1",
            "item1",
            "test",
            None,
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();

    svc.revoke_grant("user1", &grant.id).await.unwrap();

    let grants = svc.list_grants("user1").await.unwrap();
    assert!(grants.is_empty());
}

#[tokio::test]
async fn ownership_check_on_delete() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let conn = svc
        .create_connection(
            "user1",
            CreateVaultConnectionRequest {
                name: "owned by user1".into(),
                provider: VaultProviderType::Hashicorp,
                config: VaultConnectionConfig::Hashicorp {
                    address: "http://localhost:8200".into(),
                    token: "tok".into(),
                    mount_path: None,
                },
            },
        )
        .await
        .unwrap();

    let result = svc.delete_connection("user2", &conn.id).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn vault_access_log_crud() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let log = svc
        .log_access(
            "user1",
            "agent1",
            "chat1",
            "conn1",
            "item1",
            Some("GH"),
            "github",
            "Need GitHub creds",
        )
        .await
        .unwrap();

    assert_eq!(log.user_id, "user1");
    assert_eq!(log.agent_id, "agent1");
    assert_eq!(log.chat_id, "chat1");
    assert_eq!(log.env_var_prefix.as_deref(), Some("GH"));

    let access_log_repo: Arc<dyn VaultAccessLogRepository> =
        Arc::new(SurrealRepo::<VaultAccessLog>::new(db.clone()));
    let logs = access_log_repo.find_by_chat_id("chat1").await.unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].vault_item_id, "item1");

    let empty = access_log_repo.find_by_chat_id("other-chat").await.unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn once_grant_not_created() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let result = svc
        .create_grant(
            "user1",
            "agent1",
            "conn1",
            "item1",
            "github",
            None,
            &GrantDuration::Once,
        )
        .await;
    assert!(result.is_err(), "Once duration should not create a grant");

    let grants = svc.list_grants("user1").await.unwrap();
    assert!(grants.is_empty());
}

#[tokio::test]
async fn access_log_without_prefix_skipped_in_hydration() {
    let db = setup_db().await;
    let svc = build_service(&db);

    svc.log_access(
        "user1", "agent1", "chat1", "conn1", "item1",
        None, "github", "Need creds",
    )
    .await
    .unwrap();

    let env_vars = svc.hydrate_chat_env_vars("user1", "chat1").await.unwrap();
    assert!(env_vars.is_empty(), "Entries without env_var_prefix should be skipped");
}

#[tokio::test]
async fn hydrate_empty_for_other_chat() {
    let db = setup_db().await;
    let svc = build_service(&db);

    svc.log_access(
        "user1", "agent1", "chat1", "conn1", "item1",
        Some("GH"), "github", "Need creds",
    )
    .await
    .unwrap();

    let env_vars = svc.hydrate_chat_env_vars("user1", "other-chat").await.unwrap();
    assert!(env_vars.is_empty());
}
