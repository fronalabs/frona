//! End-to-end tests that spawn `fake-mcp-server` as a real child process,
//! exercise the MCP client handshake + tool invocation, and verify the manager
//! detects dead processes for the supervisor.
//!
//! The binary must be built first:
//!     cargo build -p frona --bin fake-mcp-server --features __test-bins
//!
//! These tests are `#[ignore]`d by default so they don't fail when the binary
//! isn't built. Run with:
//!     cargo test -p frona --test mcp_e2e -- --ignored

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use frona::core::repository::Repository;
use frona::tool::mcp::McpManager;
use frona::tool::mcp::models::{McpPackage, McpRuntime, McpServer, McpServerStatus};
use frona::tool::sandbox::driver::resource_monitor::SystemResourceManager;
use frona::tool::sandbox::{SandboxFactory, SandboxManager};

fn fake_server_binary() -> String {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/fake-mcp-server");
    if !path.exists() {
        panic!(
            "fake-mcp-server binary not found at {}. Build it first:\n  \
             cargo build -p frona-server --bin fake-mcp-server --features __test-bins",
            path.display()
        );
    }
    path.to_string_lossy().into_owned()
}

fn make_server(id: &str, binary: &str, workspace: &str) -> McpServer {
    let now = chrono::Utc::now();
    McpServer {
        id: id.to_string(),
        user_id: "test-user".to_string(),
        handle: frona::core::Handle::try_new(format!("fake-{id}")).expect("test handle invalid"),
        display_name: format!("Fake {id}"),
        description: None,
        repository_url: None,
        registry_id: None,
        server_info: None,
        package: McpPackage {
            runtime: McpRuntime::Binary,
            name: "fake-mcp-server".into(),
            version: "0.0.0".into(),
        },
        command: binary.to_string(),
        args: vec![],
        env: BTreeMap::new(),
        transports: vec![],
        active_transport: "stdio".into(),
        status: McpServerStatus::Installed,
        tool_cache: vec![],
        workspace_dir: workspace.to_string(),
        installed_at: now,
        last_started_at: None,
        updated_at: now,
    }
}

async fn test_manager(tmp: &std::path::Path) -> Arc<McpManager> {
    let factory = Arc::new(SandboxFactory::new(
        true,
        Arc::new(SystemResourceManager::new(80.0, 80.0, 90.0, 90.0)),
    ));
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let user_repo = frona::db::repo::generic::SurrealRepo::new(db.clone());
    let now = chrono::Utc::now();
    user_repo
        .create(&frona::auth::User {
            id: "test-user".into(),
            handle: frona::handle!("testuser"),
            email: "test@example.com".into(),
            name: "Test User".into(),
            password_hash: "unused".into(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let storage = frona::storage::StorageService::new(&frona::core::config::Config {
        storage: frona::core::config::StorageConfig {
            data_dir: tmp.to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    });
    let user_service =
        frona::auth::UserService::new(user_repo, &frona::core::config::CacheConfig::default());
    let tool_fixture = helpers::app_state::build(&db).await;
    let tool_manager = tool_fixture.state.tool_manager.clone();
    let policy_repo: Arc<dyn frona::policy::repository::PolicyRepository> =
        Arc::new(frona::db::repo::generic::SurrealRepo::<
            frona::policy::models::Policy,
        >::new(db.clone()));
    let policy_service = frona::policy::service::PolicyService::new(
        policy_repo,
        frona::policy::schema::build_schema(),
        tool_manager,
        storage.clone(),
        user_service.clone(),
    );
    let skill_service = frona::agent::skill::service::SkillService::new(
        frona::agent::skill::registry::SkillRegistryClient::new(
            frona::build_http_client(),
            "/tmp/frona-test-mcp-e2e-cache",
        ),
        frona::agent::skill::resolver::SkillResolver::new(
            "/tmp/frona-test-mcp-e2e-shared",
            storage.clone(),
            "/tmp/frona-test-mcp-e2e-skills",
        ),
        storage.clone(),
        "/tmp/frona-test-mcp-e2e-skills",
        &frona::core::config::CacheConfig::default(),
    );
    let keypair_repo: Arc<dyn frona::credential::keypair::repository::KeyPairRepository> =
        Arc::new(frona::db::repo::generic::SurrealRepo::new(db.clone()));
    let keypair_service =
        frona::credential::keypair::service::KeyPairService::new("test-secret", keypair_repo);
    let token_repo: Arc<
        frona::db::repo::generic::SurrealRepo<frona::auth::token::models::ApiToken>,
    > = Arc::new(frona::db::repo::generic::SurrealRepo::new(db));
    let token_service = frona::auth::token::service::TokenService::new(
        token_repo,
        frona::auth::jwt::JwtService::new(),
        user_service.clone(),
        3600,
        86400,
    );
    let sandbox_manager = Arc::new(SandboxManager::new(
        factory,
        policy_service,
        skill_service,
        storage.clone(),
        token_service,
        keypair_service,
        "http://localhost".to_string(),
        300,
        "UTC".to_string(),
    ));
    Arc::new(McpManager::new(
        sandbox_manager,
        storage,
        4100,
        4200,
        user_service,
        frona::build_http_client(),
    ))
}

#[tokio::test]
#[ignore = "requires fake-mcp-server binary; build with --features __test-bins"]
async fn spawn_handshake_and_tool_call() {
    let binary = fake_server_binary();
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws1");
    std::fs::create_dir_all(&workspace).unwrap();

    let manager = test_manager(tmp.path()).await;
    let server = make_server("s1", &binary, &workspace.to_string_lossy());

    let tools = manager
        .start(&server, BTreeMap::new())
        .await
        .expect("start should succeed");

    assert!(
        tools.iter().any(|t| t.id == "mcp__fake-s1__echo"),
        "echo tool should be discovered; got: {:?}",
        tools.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    assert!(
        tools.iter().any(|t| t.id == "mcp__fake-s1__add"),
        "add tool should be discovered"
    );

    let result = manager
        .call("s1", "echo", serde_json::json!({"text": "hello"}))
        .await
        .expect("echo call should succeed");
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello");

    let result = manager
        .call("s1", "add", serde_json::json!({"a": 3, "b": 7}))
        .await
        .expect("add call should succeed");
    let sum: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sum, "10");

    manager.stop("s1").await.expect("stop should succeed");
    assert!(!manager.is_running("s1").await);
}

#[tokio::test]
#[ignore = "requires fake-mcp-server binary; build with --features __test-bins"]
async fn health_check_detects_killed_process() {
    let binary = fake_server_binary();
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws2");
    std::fs::create_dir_all(&workspace).unwrap();

    let manager = test_manager(tmp.path()).await;
    let server = make_server("s2", &binary, &workspace.to_string_lossy());

    manager
        .start(&server, BTreeMap::new())
        .await
        .expect("start should succeed");
    assert!(manager.is_running("s2").await);

    // Kill the child process externally (simulating a crash)
    {
        let mut conns = manager.connections_mut().await;
        if let Some(conn) = conns.get_mut("s2")
            && let Some(ref mut child) = conn.child
        {
            child.kill().await.expect("kill should succeed");
            let _ = child.wait().await;
        }
    }

    // Give the OS a moment to reap
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let dead = manager.health_check().await;
    assert!(
        dead.contains(&"s2".to_string()),
        "health_check should report s2 as dead; got {dead:?}"
    );
}

#[tokio::test]
#[ignore = "requires fake-mcp-server binary; build with --features __test-bins"]
async fn tools_for_user_returns_filtered_tools() {
    let binary = fake_server_binary();
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws3");
    std::fs::create_dir_all(&workspace).unwrap();

    let manager = test_manager(tmp.path()).await;
    let server = make_server("s3", &binary, &workspace.to_string_lossy());

    manager
        .start(&server, BTreeMap::new())
        .await
        .expect("start should succeed");

    let mut allowlist = HashMap::new();
    allowlist.insert("fake-s3".to_string(), {
        let mut s = HashSet::new();
        s.insert("echo".to_string());
        s
    });

    let tools = manager.tools_for_user("test-user", &allowlist).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].id, "mcp__fake-s3__echo");

    manager.stop("s3").await.unwrap();
}

mod helpers;
