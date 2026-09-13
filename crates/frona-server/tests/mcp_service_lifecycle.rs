//! Lifecycle integration test for `McpServerService`:
//! install → update → uninstall, exercising grant verification, binding
//! persistence, binding replacement on update, and grant+binding sweep on
//! uninstall. Does not exercise `start` (which requires spawning a real
//! sandboxed child process).

use async_trait::async_trait;
use std::sync::Arc;

use frona::core::Principal;
use frona::core::error::AppError;
use frona::credential::vault::models::*;
use frona::credential::vault::service::VaultService;
use frona::db::init::setup_schema;
use frona::db::repo::generic::SurrealRepo;
use frona::tool::mcp::metadata::{
    RegistryEnvVar, RegistryPackage, RegistryServerEntry, RegistryTransport,
};
use frona::tool::mcp::models::{
    CredentialBinding, McpServer, McpServerInstall, McpServerStatus, McpServerUpdate,
};
use frona::tool::mcp::registry::McpRegistryClient;
use frona::tool::mcp::repository::McpServerRepository;
use frona::tool::mcp::service::{McpServerService, NoopPackageInstaller};
use frona::tool::mcp::{McpManager, PackageInstaller};

struct FakeRegistry {
    entry: RegistryServerEntry,
}

#[async_trait]
impl McpRegistryClient for FakeRegistry {
    async fn search(
        &self,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<RegistryServerEntry>, AppError> {
        Ok(vec![self.entry.clone()])
    }
    async fn fetch(&self, _name: &str) -> Result<RegistryServerEntry, AppError> {
        Ok(self.entry.clone())
    }
    async fn fetch_version(
        &self,
        _name: &str,
        _version: &str,
    ) -> Result<RegistryServerEntry, AppError> {
        Ok(self.entry.clone())
    }
}

fn sample_entry(env_vars: Vec<RegistryEnvVar>) -> RegistryServerEntry {
    RegistryServerEntry {
        name: "io.example/workspace-mcp".into(),
        description: "A fake MCP server".into(),
        version: "1.0.0".into(),
        title: Some("Workspace MCP".into()),
        repository: None,
        website_url: None,
        packages: vec![RegistryPackage {
            registry_type: "npm".into(),
            identifier: "@example/workspace-mcp".into(),
            version: Some("1.0.0".into()),
            runtime_hint: None,
            transport: RegistryTransport {
                kind: "stdio".into(),
                url: None,
            },
            runtime_arguments: vec![],
            package_arguments: vec![],
            environment_variables: env_vars,
        }],
        remotes: vec![],
        status: Default::default(),
        is_latest: true,
        status_message: None,
        status_changed_at: None,
        published_at: None,
        updated_at: None,
        enrichment: None,
        score: None,
    }
}

fn secret_env_var(name: &str) -> RegistryEnvVar {
    RegistryEnvVar {
        name: name.into(),
        description: None,
        is_required: true,
        is_secret: true,
        format: None,
    }
}

async fn build_test_harness(
    env_vars: Vec<RegistryEnvVar>,
) -> (
    surrealdb::Surreal<surrealdb::engine::local::Db>,
    VaultService,
    McpServerService,
    tempfile::TempDir,
) {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    setup_schema(&db).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();

    let test_storage = frona::storage::StorageService::new(&frona::core::config::Config {
        storage: frona::core::config::StorageConfig {
            data_dir: tmp.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    });
    let test_user_service = frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    let vault = VaultService::new(
        Arc::new(SurrealRepo::<VaultConnection>::new(db.clone())),
        Arc::new(SurrealRepo::<VaultGrant>::new(db.clone())),
        Arc::new(SurrealRepo::<Credential>::new(db.clone())),
        Arc::new(SurrealRepo::<VaultAccessLog>::new(db.clone())),
        Arc::new(SurrealRepo::<PrincipalCredentialBinding>::new(db.clone())),
        "test-secret",
        Default::default(),
        tmp.path().to_path_buf(),
        test_storage.clone(),
        test_user_service,
        frona::credential::managed::ManagedVault::new(
            Arc::new(frona::db::repo::managed_vault::SurrealManagedVaultRepo::new(db.clone())),
            "test-secret",
            "managed".into(),
        ),
        Arc::new(frona::credential::managed::resolver::ManagedResolver::new(
            std::collections::HashMap::new(),
        )),
        frona::credential::managed::login::ManagedLoginService::registered(),
    );
    vault.sync_config_connections().await.unwrap();

    let factory = Arc::new(frona::tool::sandbox::SandboxFactory::new(
        true, // sandbox_disabled: we never actually start servers in these tests
        Arc::new(
            frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                80.0, 80.0, 90.0, 90.0,
            ),
        ),
    ));
    let policy_schema = frona::policy::schema::build_schema();
    let policy_repo: Arc<dyn frona::policy::repository::PolicyRepository> = Arc::new(
        SurrealRepo::<frona::policy::models::Policy>::new(db.clone()),
    );
    let tool_fixture = helpers::app_state::build(&db).await;
    let policy_tool_manager = tool_fixture.state.tool_manager.clone();
    let storage = frona::storage::StorageService::new(&frona::core::config::Config::default());
    let user_service = frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    let policy_service = frona::policy::service::PolicyService::new(
        policy_repo,
        policy_schema,
        policy_tool_manager,
        storage.clone(),
        user_service.clone(),
    );
    let skill_service = frona::agent::skill::service::SkillService::new(
        frona::agent::skill::registry::SkillRegistryClient::new(
            frona::build_http_client(),
            "/tmp/frona-test-lifecycle-cache",
        ),
        frona::agent::skill::resolver::SkillResolver::new(
            "/tmp/frona-test-lifecycle-shared",
            storage.clone(),
            "/tmp/frona-test-lifecycle-skills",
        ),
        storage.clone(),
        "/tmp/frona-test-lifecycle-skills",
        &frona::core::config::CacheConfig::default(),
    );
    let lifecycle_keypair_service = frona::credential::keypair::service::KeyPairService::new(
        "test-secret",
        Arc::new(SurrealRepo::new(db.clone())),
    );
    let lifecycle_token_service = frona::auth::token::service::TokenService::new(
        Arc::new(SurrealRepo::new(db.clone())),
        frona::auth::jwt::JwtService::new(),
        user_service.clone(),
        3600,
        86400,
    );
    let sandbox_manager = Arc::new(frona::tool::sandbox::SandboxManager::new(
        factory,
        policy_service.clone(),
        skill_service,
        storage.clone(),
        lifecycle_token_service,
        lifecycle_keypair_service,
        vault.clone(),
        "http://localhost".to_string(),
        300,
        "UTC".to_string(),
    ));
    let manager = Arc::new(McpManager::new(
        sandbox_manager,
        test_storage,
        4100,
        4200,
        user_service,
        frona::build_http_client(),
    ));
    let mcp_repo: Arc<dyn McpServerRepository> =
        Arc::new(SurrealRepo::<McpServer>::new(db.clone()));
    let registry: Arc<dyn McpRegistryClient> = Arc::new(FakeRegistry {
        entry: sample_entry(env_vars),
    });
    let installer: Arc<dyn PackageInstaller> = Arc::new(NoopPackageInstaller);

    let keypair_service = frona::credential::keypair::service::KeyPairService::new(
        "test-secret",
        Arc::new(SurrealRepo::new(db.clone())),
    );
    let user_service =
        frona::auth::UserService::new(SurrealRepo::new(db.clone()), &Default::default());
    let token_service = frona::auth::token::service::TokenService::new(
        Arc::new(SurrealRepo::new(db.clone())),
        frona::auth::jwt::JwtService::new(),
        user_service.clone(),
        900,
        604_800,
    );
    let tool_fixture = helpers::app_state::build(&db).await;
    let tool_manager = tool_fixture.state.tool_manager.clone();

    let service = McpServerService::new(
        mcp_repo,
        manager,
        registry,
        Arc::new(vault.clone()),
        installer,
        tool_manager,
        token_service,
        keypair_service,
        user_service,
        policy_service,
        storage.clone(),
        "http://localhost".to_string(),
        300,
    );

    (db, vault, service, tmp)
}

async fn seed_credential_and_grant(
    vault: &VaultService,
    user_id: &str,
    principal: Principal,
    name: &str,
    password: &str,
) -> String {
    let cred = vault
        .create_credential(
            user_id,
            CreateLocalItemRequest::UsernamePassword {
                name: name.into(),
                username: "u".into(),
                password: password.into(),
            },
        )
        .await
        .unwrap();
    vault
        .create_grant(
            user_id,
            principal,
            "local",
            &cred.id,
            name,
            &GrantDuration::Permanent,
        )
        .await
        .unwrap();
    cred.id
}

fn binding(env_var: &str, vault_item_id: &str) -> CredentialBinding {
    CredentialBinding {
        connection_id: "local".into(),
        vault_item_id: vault_item_id.into(),
        env_var: env_var.into(),
        field: VaultField::Password,
    }
}

#[tokio::test]
async fn install_rejects_when_binding_has_no_matching_grant() {
    let (_db, _vault, service, _tmp) =
        build_test_harness(vec![secret_env_var("GITHUB_TOKEN")]).await;

    let req = McpServerInstall {
        registry_id: Some("io.example/workspace-mcp".into()),
        manifest: None,
        display_name_override: None,
        credentials: vec![binding("GITHUB_TOKEN", "nonexistent-item")],
        extra_env: Default::default(),
        sandbox_policy: None,
        handle: None,
    };
    let err = service
        .install("user1", &frona::handle!("user1"), req)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AppError::Forbidden(_)),
        "expected Forbidden for missing grant, got {err:?}"
    );
}

#[tokio::test]
async fn install_allows_missing_binding_for_declared_secret() {
    let (_db, _vault, service, _tmp) =
        build_test_harness(vec![secret_env_var("GITHUB_TOKEN")]).await;

    let req = McpServerInstall {
        registry_id: Some("io.example/workspace-mcp".into()),
        manifest: None,
        display_name_override: None,
        credentials: vec![],
        extra_env: Default::default(),
        sandbox_policy: None,
        handle: None,
    };
    let server = service
        .install("user1", &frona::handle!("user1"), req)
        .await
        .unwrap();
    assert_eq!(server.status, McpServerStatus::Installed);
}

#[tokio::test]
async fn install_rejects_extraneous_binding() {
    let (_db, _vault, service, _tmp) =
        build_test_harness(vec![secret_env_var("GITHUB_TOKEN")]).await;

    let req = McpServerInstall {
        registry_id: Some("io.example/workspace-mcp".into()),
        manifest: None,
        display_name_override: None,
        credentials: vec![
            binding("GITHUB_TOKEN", "item"),
            binding("NOT_DECLARED", "item2"),
        ],
        extra_env: Default::default(),
        sandbox_policy: None,
        handle: None,
    };
    let err = service
        .install("user1", &frona::handle!("user1"), req)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AppError::Validation(_)),
        "expected Validation for extraneous binding, got {err:?}"
    );
}

#[tokio::test]
async fn install_rejects_relative_extra_paths() {
    let (_db, _vault, service, _tmp) = build_test_harness(vec![]).await;

    let req = McpServerInstall {
        registry_id: Some("io.example/workspace-mcp".into()),
        manifest: None,
        display_name_override: None,
        credentials: vec![],
        extra_env: Default::default(),
        sandbox_policy: Some(frona::policy::sandbox::SandboxPolicy {
            read_paths: vec!["relative/path".into()],
            ..Default::default()
        }),
        handle: None,
    };
    let err = service
        .install("user1", &frona::handle!("user1"), req)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Validation(_)));
}

#[tokio::test]
async fn install_succeeds_with_empty_env_entry() {
    let (db, _vault, service, _tmp) = build_test_harness(vec![]).await;

    let req = McpServerInstall {
        registry_id: Some("io.example/workspace-mcp".into()),
        manifest: None,
        display_name_override: None,
        credentials: vec![],
        extra_env: Default::default(),
        sandbox_policy: None,
        handle: None,
    };
    let persisted = service
        .install("user1", &frona::handle!("user1"), req)
        .await
        .unwrap();
    assert_eq!(persisted.user_id, "user1");
    // Derived from registry entry title (which the test harness sets to the
    // package id's tail) via sanitize_to_handle.
    assert_eq!(persisted.handle.as_str(), "workspace-mcp");
    assert_eq!(persisted.command, "npx");
    assert_eq!(
        persisted.args,
        vec!["--yes", "@example/workspace-mcp@1.0.0"]
    );

    let mcp_repo: Arc<dyn McpServerRepository> = Arc::new(SurrealRepo::<McpServer>::new(db));
    let list = mcp_repo.find_by_user("user1").await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn uninstall_sweeps_bindings_and_grants() {
    let (_db, vault, service, _tmp) = build_test_harness(vec![]).await;

    let persisted = service
        .install(
            "user1",
            &frona::handle!("user1"),
            McpServerInstall {
                registry_id: Some("io.example/workspace-mcp".into()),
                manifest: None,
                display_name_override: None,
                credentials: vec![],
                extra_env: Default::default(),
                sandbox_policy: None,
                handle: None,
            },
        )
        .await
        .unwrap();
    let principal = Principal::mcp_server(&persisted.id);

    // Post-install, write a grant + binding directly against the server's
    // principal to prove uninstall sweeps them even if they were added later.
    let cred_id =
        seed_credential_and_grant(&vault, "user1", principal.clone(), "gh", "ghp_xxx").await;
    vault
        .create_binding(
            "user1",
            principal.clone(),
            "gh",
            "local",
            &cred_id,
            CredentialTarget::Single {
                env_var: "GH".into(),
                field: VaultField::Password,
            },
            BindingScope::Durable,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        vault
            .list_bindings_for_principal("user1", &principal)
            .await
            .unwrap()
            .len(),
        1
    );

    service.uninstall("user1", &persisted.id).await.unwrap();

    assert!(
        vault
            .list_bindings_for_principal("user1", &principal)
            .await
            .unwrap()
            .is_empty(),
        "bindings should be swept on uninstall"
    );
    let remaining_grants = vault
        .list_bindings_for_principal("user1", &principal)
        .await
        .unwrap();
    assert!(
        remaining_grants.is_empty(),
        "grants should be swept on uninstall"
    );
}

#[tokio::test]
async fn update_extra_env_replaces_value() {
    let (_db, _vault, service, _tmp) = build_test_harness(vec![]).await;

    let persisted = service
        .install(
            "user1",
            &frona::handle!("user1"),
            McpServerInstall {
                registry_id: Some("io.example/workspace-mcp".into()),
                manifest: None,
                display_name_override: None,
                credentials: vec![],
                extra_env: [("LOG_LEVEL".to_string(), "info".to_string())]
                    .into_iter()
                    .collect(),
                sandbox_policy: None,
                handle: None,
            },
        )
        .await
        .unwrap();

    let update = McpServerUpdate {
        extra_env: Some(
            [("LOG_LEVEL".to_string(), "debug".to_string())]
                .into_iter()
                .collect(),
        ),
        ..Default::default()
    };
    let result = service
        .update("user1", &persisted.id, update)
        .await
        .unwrap();
    assert_eq!(
        result.server.env.get("LOG_LEVEL").map(String::as_str),
        Some("debug")
    );
    assert!(
        !result.restart_required,
        "not running, so no restart needed"
    );
}

#[tokio::test]
async fn update_rejects_when_another_user_owns_the_server() {
    let (_db, _vault, service, _tmp) = build_test_harness(vec![]).await;

    let persisted = service
        .install(
            "owner",
            &frona::handle!("owner"),
            McpServerInstall {
                registry_id: Some("io.example/workspace-mcp".into()),
                manifest: None,
                display_name_override: None,
                credentials: vec![],
                extra_env: Default::default(),
                sandbox_policy: None,
                handle: None,
            },
        )
        .await
        .unwrap();

    let result = service
        .update("attacker", &persisted.id, McpServerUpdate::default())
        .await;
    assert!(
        matches!(result, Err(AppError::Forbidden(_))),
        "non-owner update should return Forbidden"
    );
}

mod helpers;
