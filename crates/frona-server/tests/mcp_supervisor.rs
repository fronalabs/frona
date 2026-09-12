//! Integration tests for `McpSupervisor` verifying the adapter delegates
//! correctly to `McpServerService` and `McpManager` against a real in-memory DB.

use std::sync::Arc;

use frona::core::error::AppError;
use frona::core::supervisor::Supervisor;
use frona::credential::vault::models::*;
use frona::credential::vault::service::VaultService;
use frona::db::init::setup_schema;
use frona::db::repo::generic::SurrealRepo;
use frona::tool::mcp::models::{McpPackage, McpRuntime, McpServer, McpServerStatus};
use frona::tool::mcp::repository::McpServerRepository;
use frona::tool::mcp::supervisor::McpSupervisor;
use frona::tool::mcp::{
    McpManager, McpRegistryClient, McpServerService, NoopPackageInstaller, PackageInstaller,
    PrebuiltMcpRegistryClient,
};
use std::collections::BTreeMap;

async fn build_mcp_supervisor() -> (
    McpSupervisor,
    Arc<dyn McpServerRepository>,
    tempfile::TempDir,
) {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    setup_schema(&db).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let storage_for_mcp = frona::storage::StorageService::new(&frona::core::config::Config {
        storage: frona::core::config::StorageConfig {
            data_dir: tmp.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    });

    let factory = Arc::new(frona::tool::sandbox::SandboxFactory::new(
        true,
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
    let policy_tool_manager = Arc::new(frona::tool::manager::ToolManager::new(false));
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
    let supervisor_skill_service = frona::agent::skill::service::SkillService::new(
        frona::agent::skill::registry::SkillRegistryClient::new(
            frona::build_http_client(),
            "/tmp/frona-test-supervisor-cache",
        ),
        frona::agent::skill::resolver::SkillResolver::new(
            "/tmp/frona-test-supervisor-shared",
            storage.clone(),
            "/tmp/frona-test-supervisor-skills",
        ),
        storage.clone(),
        "/tmp/frona-test-supervisor-skills",
        &frona::core::config::CacheConfig::default(),
    );
    let supervisor_keypair_service = frona::credential::keypair::service::KeyPairService::new(
        "test-secret",
        Arc::new(SurrealRepo::new(db.clone())),
    );
    let supervisor_token_service = frona::auth::token::service::TokenService::new(
        Arc::new(SurrealRepo::new(db.clone())),
        frona::auth::jwt::JwtService::new(),
        user_service.clone(),
        3600,
        86400,
    );
    let sandbox_manager = Arc::new(frona::tool::sandbox::SandboxManager::new(
        factory,
        policy_service.clone(),
        supervisor_skill_service,
        storage.clone(),
        supervisor_token_service,
        supervisor_keypair_service,
        "http://localhost".to_string(),
        300,
        "UTC".to_string(),
    ));
    let manager = Arc::new(McpManager::new(
        sandbox_manager,
        storage_for_mcp.clone(),
        4100,
        4200,
        user_service.clone(),
        frona::build_http_client(),
    ));
    let mcp_repo: Arc<dyn McpServerRepository> =
        Arc::new(SurrealRepo::<McpServer>::new(db.clone()));
    let vault_storage = frona::storage::StorageService::new(&frona::core::config::Config {
        storage: frona::core::config::StorageConfig {
            data_dir: tmp.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    });
    let vault = VaultService::new(
        Arc::new(SurrealRepo::<VaultConnection>::new(db.clone())),
        Arc::new(SurrealRepo::<VaultGrant>::new(db.clone())),
        Arc::new(SurrealRepo::<Credential>::new(db.clone())),
        Arc::new(SurrealRepo::<VaultAccessLog>::new(db.clone())),
        Arc::new(SurrealRepo::<PrincipalCredentialBinding>::new(db.clone())),
        "test-secret",
        Default::default(),
        tmp.path().to_path_buf(),
        vault_storage,
        user_service.clone(),
    );
    let registry: Arc<dyn McpRegistryClient> = Arc::new(PrebuiltMcpRegistryClient::new(
        frona::build_http_client(),
        tmp.path().join("registry"),
    ));
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
    let tool_manager = Arc::new(frona::tool::manager::ToolManager::new(false));

    let service = Arc::new(McpServerService::new(
        mcp_repo.clone(),
        manager.clone(),
        registry,
        Arc::new(vault),
        installer,
        tool_manager,
        token_service,
        keypair_service,
        user_service,
        policy_service,
        storage_for_mcp.clone(),
        "http://localhost".to_string(),
        300,
    ));

    let supervisor = McpSupervisor::new(service, manager);
    (supervisor, mcp_repo, tmp)
}

fn make_server(id: &str, user_id: &str, status: McpServerStatus) -> McpServer {
    let now = chrono::Utc::now();
    McpServer {
        id: id.to_string(),
        user_id: user_id.to_string(),
        handle: frona::core::Handle::try_new(format!("test-{id}")).expect("test handle invalid"),
        display_name: format!("Test Server {id}"),
        description: None,
        repository_url: None,
        registry_id: None,
        server_info: None,
        package: McpPackage {
            runtime: McpRuntime::Npm,
            name: "@example/test".into(),
            version: "1.0.0".into(),
        },
        command: "echo".into(),
        args: vec![],
        env: BTreeMap::new(),
        transports: vec![],
        active_transport: "stdio".into(),
        status,
        tool_cache: vec![],
        workspace_dir: "/tmp/test".into(),
        installed_at: now,
        last_started_at: None,
        updated_at: now,
    }
}

#[tokio::test]
async fn find_running_returns_running_servers_only() {
    let (supervisor, repo, _tmp) = build_mcp_supervisor().await;

    repo.create(&make_server("s1", "u1", McpServerStatus::Running))
        .await
        .unwrap();
    repo.create(&make_server("s2", "u1", McpServerStatus::Installed))
        .await
        .unwrap();
    repo.create(&make_server("s3", "u1", McpServerStatus::Running))
        .await
        .unwrap();

    let running = supervisor.find_running().await.unwrap();
    assert_eq!(running.len(), 2);
    assert!(running.contains(&"s1".to_string()));
    assert!(running.contains(&"s3".to_string()));
}

#[tokio::test]
async fn mark_failed_updates_db_status() {
    let (supervisor, repo, _tmp) = build_mcp_supervisor().await;

    repo.create(&make_server("s1", "u1", McpServerStatus::Running))
        .await
        .unwrap();

    supervisor
        .mark_failed("s1", "too many restarts")
        .await
        .unwrap();

    let server = repo.find_by_id("s1").await.unwrap().unwrap();
    assert!(matches!(server.status, McpServerStatus::Failed));
}

#[tokio::test]
async fn owner_of_returns_user_id_from_db() {
    let (supervisor, repo, _tmp) = build_mcp_supervisor().await;

    repo.create(&make_server("s1", "owner-42", McpServerStatus::Running))
        .await
        .unwrap();

    assert_eq!(supervisor.owner_of("s1").await.unwrap(), "owner-42");
}

#[tokio::test]
async fn display_name_returns_server_display_name() {
    let (supervisor, repo, _tmp) = build_mcp_supervisor().await;

    repo.create(&make_server("s1", "u1", McpServerStatus::Running))
        .await
        .unwrap();

    assert_eq!(supervisor.display_name("s1").await, "Test Server s1");
}

#[tokio::test]
async fn owner_of_missing_server_returns_not_found() {
    let (supervisor, _repo, _tmp) = build_mcp_supervisor().await;
    let result = supervisor.owner_of("nonexistent").await;
    assert!(matches!(result, Err(AppError::NotFound(_))));
}

#[tokio::test]
async fn label_is_mcp() {
    let (supervisor, _repo, _tmp) = build_mcp_supervisor().await;
    assert_eq!(supervisor.label(), "mcp");
}
