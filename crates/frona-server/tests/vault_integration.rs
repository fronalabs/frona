use frona::core::Principal;
use frona::core::config::VaultConfig;
use frona::credential::vault::models::*;
use frona::credential::vault::repository::{
    VaultAccessLogRepository, VaultConnectionRepository, VaultGrantRepository,
};
use frona::credential::vault::service::VaultService;
use frona::db::init::setup_schema;
use frona::db::repo::generic::SurrealRepo;
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
    let binding_repo: Arc<
        dyn frona::credential::vault::repository::PrincipalCredentialBindingRepository,
    > = Arc::new(SurrealRepo::<PrincipalCredentialBinding>::new(db.clone()));
    let storage = frona::storage::StorageService::new(&frona::core::config::Config {
        storage: frona::core::config::StorageConfig {
            data_dir: "/tmp/test-data".to_string(),
            ..Default::default()
        },
        ..Default::default()
    });
    let user_service = frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    VaultService::new(
        connection_repo,
        grant_repo,
        credential_repo,
        access_log_repo,
        binding_repo,
        "test-secret",
        VaultConfig::default(),
        std::path::PathBuf::from("/tmp/test-data"),
        storage,
        user_service,
        frona::credential::managed::ManagedVault::new(
            Arc::new(frona::db::repo::managed_vault::SurrealManagedVaultRepo::new(db.clone())),
            "test-secret",
            "managed".into(),
        ),
        Arc::new(frona::credential::managed::resolver::ManagedResolver::new(
            std::collections::HashMap::new(),
        )),
        frona::credential::managed::login::ManagedLoginService::registered(),
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
        Principal::agent("agent1"),
        &conn.id,
        "item1",
        "github",
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
        Principal::agent("agent1"),
        &conn.id,
        "item1",
        "github",
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();

    let found = svc
        .find_matching_grant("user1", &Principal::agent("agent1"), "github")
        .await
        .unwrap();
    assert!(found.is_some());

    let not_found = svc
        .find_matching_grant("user1", &Principal::agent("agent1"), "gitlab")
        .await
        .unwrap();
    assert!(not_found.is_none());
}

#[tokio::test]
async fn expired_grant_is_cleaned_up() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    let grant_repo: Arc<dyn VaultGrantRepository> =
        Arc::new(SurrealRepo::<VaultGrant>::new(db.clone()));

    let expired_grant = VaultGrant {
        id: frona::core::repository::new_id(),
        user_id: "user1".into(),
        connection_id: conn.id,
        vault_item_id: "item1".into(),
        principal: Principal::agent("agent1"),
        query: "old-service".into(),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        created_at: chrono::Utc::now(),
    };
    grant_repo.create(&expired_grant).await.unwrap();

    let result = svc
        .find_matching_grant("user1", &Principal::agent("agent1"), "old-service")
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

    let toggled = svc
        .toggle_connection("user1", &conn.id, false)
        .await
        .unwrap();
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
async fn find_by_principal_returns_only_matching_scope() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    svc.create_grant(
        "user1",
        Principal::agent("agent1"),
        &conn.id,
        "item_a",
        "github",
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();
    svc.create_grant(
        "user1",
        Principal::mcp_server("srv1"),
        &conn.id,
        "item_b",
        "gmail",
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();
    svc.create_grant(
        "user1",
        Principal::mcp_server("srv2"),
        &conn.id,
        "item_c",
        "slack",
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();

    let grant_repo: Arc<dyn VaultGrantRepository> =
        Arc::new(SurrealRepo::<VaultGrant>::new(db.clone()));

    let mcp1_grants = grant_repo
        .find_by_principal("user1", &Principal::mcp_server("srv1"))
        .await
        .unwrap();
    assert_eq!(mcp1_grants.len(), 1);
    assert_eq!(mcp1_grants[0].vault_item_id, "item_b");

    let agent_grants = grant_repo
        .find_by_principal("user1", &Principal::agent("agent1"))
        .await
        .unwrap();
    assert_eq!(agent_grants.len(), 1);
    assert_eq!(agent_grants[0].vault_item_id, "item_a");

    let no_grants = grant_repo
        .find_by_principal("user1", &Principal::mcp_server("ghost"))
        .await
        .unwrap();
    assert!(no_grants.is_empty());
}

#[tokio::test]
async fn delete_by_principal_sweeps_only_matching_scope() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    svc.create_grant(
        "user1",
        Principal::mcp_server("srv1"),
        &conn.id,
        "item_a",
        "github",
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();
    svc.create_grant(
        "user1",
        Principal::mcp_server("srv1"),
        &conn.id,
        "item_b",
        "gmail",
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();
    svc.create_grant(
        "user1",
        Principal::agent("agent1"),
        &conn.id,
        "item_c",
        "untouched",
        &GrantDuration::Permanent,
    )
    .await
    .unwrap();

    let grant_repo: Arc<dyn VaultGrantRepository> =
        Arc::new(SurrealRepo::<VaultGrant>::new(db.clone()));

    grant_repo
        .delete_by_principal("user1", &Principal::mcp_server("srv1"))
        .await
        .unwrap();

    let remaining = svc.list_grants("user1").await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].query, "untouched");
}

#[tokio::test]
async fn revoke_grant() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let grant = svc
        .create_grant(
            "user1",
            Principal::agent("agent1"),
            "conn1",
            "item1",
            "test",
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
            Principal::agent("agent1"),
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
    assert_eq!(log.principal, Principal::agent("agent1"));
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
            Principal::agent("agent1"),
            "conn1",
            "item1",
            "github",
            &GrantDuration::Once,
        )
        .await;
    assert!(result.is_err(), "Once duration should not create a grant");

    let grants = svc.list_grants("user1").await.unwrap();
    assert!(grants.is_empty());
}

#[tokio::test]
async fn hydrate_returns_empty_when_no_bindings() {
    let db = setup_db().await;
    let svc = build_service(&db);

    let env_vars = svc
        .hydrate_chat_env_vars("user1", "chat1", "agent1")
        .await
        .unwrap();
    assert!(env_vars.is_empty());
}

#[tokio::test]
async fn hydrate_projects_durable_bindings_into_env_vars() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let credential = svc
        .create_credential(
            "user1",
            CreateLocalItemRequest::UsernamePassword {
                name: "GitHub".into(),
                username: "octocat".into(),
                password: "ghp_durable".into(),
            },
        )
        .await
        .unwrap();

    svc.create_binding(
        "user1",
        Principal::agent("agent1"),
        "github",
        "local",
        &credential.id,
        CredentialTarget::Prefix {
            env_var_prefix: "GH".into(),
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    let env: std::collections::HashMap<String, String> = svc
        .hydrate_chat_env_vars("user1", "any-chat", "agent1")
        .await
        .unwrap()
        .into_iter()
        .collect();

    assert_eq!(env.get("GH_USERNAME").map(String::as_str), Some("octocat"));
    assert_eq!(
        env.get("GH_PASSWORD").map(String::as_str),
        Some("ghp_durable")
    );
}

#[tokio::test]
async fn hydrate_honors_chat_scope_isolation() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let cred = svc
        .create_credential(
            "user1",
            CreateLocalItemRequest::UsernamePassword {
                name: "X".into(),
                username: "u".into(),
                password: "p".into(),
            },
        )
        .await
        .unwrap();

    svc.create_binding(
        "user1",
        Principal::agent("agent1"),
        "x",
        "local",
        &cred.id,
        CredentialTarget::Prefix {
            env_var_prefix: "X".into(),
        },
        BindingScope::Chat {
            chat_id: "chat1".into(),
        },
        None,
    )
    .await
    .unwrap();

    let in_chat = svc
        .hydrate_chat_env_vars("user1", "chat1", "agent1")
        .await
        .unwrap();
    assert!(!in_chat.is_empty(), "chat1 should see its own binding");

    let other_chat = svc
        .hydrate_chat_env_vars("user1", "chat2", "agent1")
        .await
        .unwrap();
    assert!(
        other_chat.is_empty(),
        "chat2 must not see chat1's chat-scoped binding"
    );
}

#[tokio::test]
async fn binding_lookup_prefers_chat_scope_over_durable() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    let principal = Principal::agent("agent1");
    svc.create_binding(
        "user1",
        principal.clone(),
        "github",
        &conn.id,
        "item_durable",
        CredentialTarget::Prefix {
            env_var_prefix: "GH".into(),
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();
    svc.create_binding(
        "user1",
        principal.clone(),
        "github",
        &conn.id,
        "item_chat",
        CredentialTarget::Prefix {
            env_var_prefix: "GH".into(),
        },
        BindingScope::Chat {
            chat_id: "chat1".into(),
        },
        None,
    )
    .await
    .unwrap();

    let chat_match = svc
        .find_binding("user1", &principal, "github", Some("chat1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chat_match.vault_item_id, "item_chat");

    let other_chat_match = svc
        .find_binding("user1", &principal, "github", Some("other-chat"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        other_chat_match.vault_item_id, "item_durable",
        "chat-scoped binding for chat1 must not leak into other chats"
    );

    let no_chat_filter = svc
        .find_binding("user1", &principal, "github", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(no_chat_filter.vault_item_id, "item_durable");
}

#[tokio::test]
async fn deleting_a_chat_cascades_into_its_chat_scoped_bindings() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    db.query("CREATE chat:ch1 CONTENT { user_id: 'user1', agent_id: 'agent1', title: 't', created_at: time::now(), updated_at: time::now() }")
        .await
        .unwrap();

    svc.create_binding(
        "user1",
        Principal::agent("agent1"),
        "github",
        &conn.id,
        "item_chat",
        CredentialTarget::Prefix {
            env_var_prefix: "GH".into(),
        },
        BindingScope::Chat {
            chat_id: "ch1".into(),
        },
        None,
    )
    .await
    .unwrap();
    svc.create_binding(
        "user1",
        Principal::agent("agent1"),
        "github-durable",
        &conn.id,
        "item_durable",
        CredentialTarget::Prefix {
            env_var_prefix: "GHD".into(),
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    let before = svc
        .list_bindings_for_principal("user1", &Principal::agent("agent1"))
        .await
        .unwrap();
    assert_eq!(before.len(), 2);

    db.query("DELETE chat:ch1").await.unwrap().check().unwrap();

    let after = svc
        .list_bindings_for_principal("user1", &Principal::agent("agent1"))
        .await
        .unwrap();
    assert_eq!(
        after.len(),
        1,
        "chat-scoped binding should be swept when its chat is deleted"
    );
    assert_eq!(after[0].vault_item_id, "item_durable");
}

#[tokio::test]
async fn delete_bindings_for_principal_sweeps_only_matching_principal() {
    let db = setup_db().await;
    let svc = build_service(&db);
    let conn = create_test_connection(&svc, "user1").await;

    svc.create_binding(
        "user1",
        Principal::agent("agent1"),
        "q",
        &conn.id,
        "i1",
        CredentialTarget::Prefix {
            env_var_prefix: "P".into(),
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();
    svc.create_binding(
        "user1",
        Principal::mcp_server("srv1"),
        "q",
        &conn.id,
        "i2",
        CredentialTarget::Prefix {
            env_var_prefix: "P".into(),
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    svc.delete_bindings_for_principal("user1", &Principal::mcp_server("srv1"))
        .await
        .unwrap();

    let agent_remaining = svc
        .list_bindings_for_principal("user1", &Principal::agent("agent1"))
        .await
        .unwrap();
    assert_eq!(agent_remaining.len(), 1);

    let mcp_remaining = svc
        .list_bindings_for_principal("user1", &Principal::mcp_server("srv1"))
        .await
        .unwrap();
    assert!(mcp_remaining.is_empty());
}

#[tokio::test]
async fn grant_with_prefix_binding_creates_and_revokes_together() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let cred = svc
        .create_credential(
            "user1",
            CreateLocalItemRequest::UsernamePassword {
                name: "GitHub".into(),
                username: "octocat".into(),
                password: "ghp_xxx".into(),
            },
        )
        .await
        .unwrap();

    let principal = Principal::mcp_server("srv1");
    let grant = svc
        .create_grant(
            "user1",
            principal.clone(),
            "local",
            &cred.id,
            "GITHUB",
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();
    svc.create_binding(
        "user1",
        principal.clone(),
        "GITHUB",
        "local",
        &cred.id,
        CredentialTarget::Prefix {
            env_var_prefix: "GITHUB".into(),
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    let bindings = svc
        .list_bindings_for_principal("user1", &principal)
        .await
        .unwrap();
    assert_eq!(bindings.len(), 1);
    assert!(matches!(
        bindings[0].target,
        CredentialTarget::Prefix { .. }
    ));

    let grants = svc.list_grants("user1").await.unwrap();
    assert_eq!(grants.len(), 1);
    assert!(grants[0].target.is_some());

    svc.revoke_grant("user1", &grant.id).await.unwrap();

    let grants_after = svc.list_grants("user1").await.unwrap();
    assert!(grants_after.is_empty());
    let bindings_after = svc
        .list_bindings_for_principal("user1", &principal)
        .await
        .unwrap();
    assert!(
        bindings_after.is_empty(),
        "revoking grant must also remove its binding"
    );
}

#[tokio::test]
async fn grant_with_single_field_binding_creates_and_revokes_together() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let cred = svc
        .create_credential(
            "user1",
            CreateLocalItemRequest::UsernamePassword {
                name: "HA Token".into(),
                username: "admin".into(),
                password: "secret_token".into(),
            },
        )
        .await
        .unwrap();

    let principal = Principal::mcp_server("ha-mcp");
    let grant = svc
        .create_grant(
            "user1",
            principal.clone(),
            "local",
            &cred.id,
            "HA_TOKEN",
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();
    svc.create_binding(
        "user1",
        principal.clone(),
        "HA_TOKEN",
        "local",
        &cred.id,
        CredentialTarget::Single {
            env_var: "HA_TOKEN".into(),
            field: VaultField::Password,
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    let bindings = svc
        .list_bindings_for_principal("user1", &principal)
        .await
        .unwrap();
    assert_eq!(bindings.len(), 1);
    assert!(matches!(
        bindings[0].target,
        CredentialTarget::Single { .. }
    ));

    let grants = svc.list_grants("user1").await.unwrap();
    assert_eq!(grants.len(), 1);
    match &grants[0].target {
        Some(CredentialTarget::Single { env_var, .. }) => assert_eq!(env_var, "HA_TOKEN"),
        other => panic!("expected Single target, got {other:?}"),
    }

    svc.revoke_grant("user1", &grant.id).await.unwrap();

    assert!(svc.list_grants("user1").await.unwrap().is_empty());
    assert!(
        svc.list_bindings_for_principal("user1", &principal)
            .await
            .unwrap()
            .is_empty(),
        "revoking grant must also remove single-field binding"
    );
}

#[tokio::test]
async fn grant_with_custom_field_binding() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let cred = svc
        .create_credential(
            "user1",
            CreateLocalItemRequest::ApiKey {
                name: "Home Assistant".into(),
                api_key: "ha_long_lived_token".into(),
            },
        )
        .await
        .unwrap();

    let principal = Principal::mcp_server("ha-srv");
    let grant = svc
        .create_grant(
            "user1",
            principal.clone(),
            "local",
            &cred.id,
            "HA_TOKEN",
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();
    svc.create_binding(
        "user1",
        principal.clone(),
        "HA_TOKEN",
        "local",
        &cred.id,
        CredentialTarget::Single {
            env_var: "HA_TOKEN".into(),
            field: VaultField::Custom {
                name: "API_KEY".into(),
            },
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    let grants = svc.list_grants("user1").await.unwrap();
    assert_eq!(grants.len(), 1);
    match &grants[0].target {
        Some(CredentialTarget::Single {
            field: VaultField::Custom { name },
            ..
        }) => assert_eq!(name, "API_KEY"),
        other => panic!("expected Single with Custom field, got {other:?}"),
    }

    svc.revoke_grant("user1", &grant.id).await.unwrap();
    assert!(
        svc.list_bindings_for_principal("user1", &principal)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn revoke_grant_only_removes_matching_binding() {
    let db = setup_db().await;
    let svc = build_service(&db);
    svc.sync_config_connections().await.unwrap();

    let cred1 = svc
        .create_credential(
            "user1",
            CreateLocalItemRequest::ApiKey {
                name: "Sonarr".into(),
                api_key: "sonarr_key".into(),
            },
        )
        .await
        .unwrap();
    let cred2 = svc
        .create_credential(
            "user1",
            CreateLocalItemRequest::ApiKey {
                name: "Radarr".into(),
                api_key: "radarr_key".into(),
            },
        )
        .await
        .unwrap();

    let principal = Principal::mcp_server("arr-srv");
    let grant1 = svc
        .create_grant(
            "user1",
            principal.clone(),
            "local",
            &cred1.id,
            "SONARR_API_KEY",
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();
    svc.create_binding(
        "user1",
        principal.clone(),
        "SONARR_API_KEY",
        "local",
        &cred1.id,
        CredentialTarget::Single {
            env_var: "SONARR_API_KEY".into(),
            field: VaultField::Custom {
                name: "API_KEY".into(),
            },
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    let _grant2 = svc
        .create_grant(
            "user1",
            principal.clone(),
            "local",
            &cred2.id,
            "RADARR_API_KEY",
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();
    svc.create_binding(
        "user1",
        principal.clone(),
        "RADARR_API_KEY",
        "local",
        &cred2.id,
        CredentialTarget::Single {
            env_var: "RADARR_API_KEY".into(),
            field: VaultField::Custom {
                name: "API_KEY".into(),
            },
        },
        BindingScope::Durable,
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        svc.list_bindings_for_principal("user1", &principal)
            .await
            .unwrap()
            .len(),
        2
    );

    svc.revoke_grant("user1", &grant1.id).await.unwrap();

    let remaining_grants = svc.list_grants("user1").await.unwrap();
    assert_eq!(remaining_grants.len(), 1);
    assert_eq!(remaining_grants[0].query, "RADARR_API_KEY");

    let remaining_bindings = svc
        .list_bindings_for_principal("user1", &principal)
        .await
        .unwrap();
    assert_eq!(
        remaining_bindings.len(),
        1,
        "only sonarr binding should be removed"
    );
    assert_eq!(remaining_bindings[0].query, "RADARR_API_KEY");
}
