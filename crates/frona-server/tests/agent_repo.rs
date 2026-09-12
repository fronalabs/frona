use chrono::Utc;
use frona::agent::models::Agent;
use frona::agent::repository::AgentRepository;
use frona::agent::service::AgentService;
use frona::core::config::CacheConfig;
use frona::core::repository::Repository;
use frona::db::init as db;
use frona::db::repo::agents::SurrealAgentRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::policy::service::PolicyService;
use frona::tool::sandbox::driver::resource_monitor::SystemResourceManager;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

fn test_resource_manager() -> Arc<SystemResourceManager> {
    Arc::new(SystemResourceManager::new(80.0, 80.0, 90.0, 90.0))
}

async fn test_policy_service(db: &Surreal<Db>) -> PolicyService {
    let schema = frona::policy::schema::build_schema();
    let repo: Arc<dyn frona::policy::repository::PolicyRepository> =
        Arc::new(SurrealRepo::<frona::policy::models::Policy>::new(
            db.clone(),
        ));
    let storage = frona::storage::StorageService::new(&frona::core::config::Config::default());
    let user_service = test_user_service(db);
    let tool_fixture = helpers::app_state::build(db).await;
    PolicyService::new(
        repo,
        schema,
        tool_fixture.state.tool_manager.clone(),
        storage,
        user_service,
    )
}

fn test_user_service(db: &Surreal<Db>) -> frona::auth::UserService {
    frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    )
}

async fn seed_user(user_service: &frona::auth::UserService, id: &str) {
    let _ = user_service
        .create(&frona::auth::User {
            id: id.into(),
            handle: frona::core::Handle::try_new(id).expect("test user id must be valid handle"),
            email: format!("{id}@example.com"),
            name: id.into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
}

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_agent(user_id: &str) -> Agent {
    let now = Utc::now();
    Agent {
        id: frona::core::repository::new_id(),
        user_id: user_id.to_string(),
        handle: frona::core::Handle::try_new(format!(
            "h-{}",
            &frona::core::repository::new_id().replace('-', "")[..28]
        ))
        .unwrap(),
        name: "Test Agent".to_string(),
        description: "A test agent".to_string(),
        model_group: "primary".to_string(),
        enabled: true,
        skills: None,
        sandbox_limits: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: std::collections::BTreeMap::new(),
        prompt: None,
        heartbeat_interval: None,
        next_heartbeat_at: None,
        heartbeat_chat_id: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn test_create_and_find_by_id() {
    let db = test_db().await;
    let repo = SurrealAgentRepo::new(db);
    let agent = test_agent("user-1");

    let created = repo.create(&agent).await.unwrap();
    assert_eq!(created.id, agent.id);
    assert_eq!(created.user_id, agent.user_id);
    assert_eq!(created.name, agent.name);
    assert_eq!(created.description, agent.description);
    assert_eq!(created.model_group, agent.model_group);
    assert_eq!(created.enabled, agent.enabled);
    assert_eq!(created.created_at, agent.created_at);
    assert_eq!(created.updated_at, agent.updated_at);

    let found = repo.find_by_id(&agent.id).await.unwrap().unwrap();
    assert_eq!(found.id, agent.id);
    assert_eq!(found.name, agent.name);
    assert_eq!(found.created_at, agent.created_at);
    assert_eq!(found.updated_at, agent.updated_at);
}

#[tokio::test]
async fn test_find_by_user_id() {
    let db = test_db().await;
    let repo = SurrealAgentRepo::new(db);

    let agent1 = test_agent("user-1");
    let mut agent2 = test_agent("user-1");
    agent2.name = "Agent 2".to_string();
    let agent3 = test_agent("user-2");

    repo.create(&agent1).await.unwrap();
    repo.create(&agent2).await.unwrap();
    repo.create(&agent3).await.unwrap();

    let agents = repo.find_by_user_id("user-1").await.unwrap();
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().all(|a| a.user_id == "user-1"));

    let agents = repo.find_by_user_id("user-2").await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].id, agent3.id);
}

#[tokio::test]
async fn test_update() {
    let db = test_db().await;
    let repo = SurrealAgentRepo::new(db);
    let agent = test_agent("user-1");

    repo.create(&agent).await.unwrap();

    let mut updated_agent = agent.clone();
    updated_agent.name = "Updated Agent".to_string();
    updated_agent.enabled = false;
    updated_agent.updated_at = Utc::now();

    let result = repo.update(&updated_agent).await.unwrap();
    assert_eq!(result.name, "Updated Agent");
    assert!(!result.enabled);

    let found = repo.find_by_id(&agent.id).await.unwrap().unwrap();
    assert_eq!(found.name, "Updated Agent");
    assert!(!found.enabled);
}

#[tokio::test]
async fn test_delete() {
    let db = test_db().await;
    let repo = SurrealAgentRepo::new(db);
    let agent = test_agent("user-1");

    repo.create(&agent).await.unwrap();
    assert!(repo.find_by_id(&agent.id).await.unwrap().is_some());

    repo.delete(&agent.id).await.unwrap();
    assert!(repo.find_by_id(&agent.id).await.unwrap().is_none());
}

#[tokio::test]
async fn test_find_by_id_not_found() {
    let db = test_db().await;
    let repo = SurrealAgentRepo::new(db);

    let found = repo.find_by_id("nonexistent-id").await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn test_clone_all_builtins_materializes_per_user_rows() {
    use frona::core::config::Config;
    use frona::storage::StorageService;

    let db = test_db().await;
    let shared_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("resources");
    let config = Config {
        storage: frona::core::config::StorageConfig {
            data_dir: "/tmp/frona_test_clone_builtins".to_string(),
            shared_config_dir: shared_dir.to_string_lossy().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = StorageService::new(&config);
    let user_service = test_user_service(&db);
    user_service
        .create(&frona::auth::User {
            id: "user-a".into(),
            handle: frona::handle!("user-a"),
            email: "a@example.com".into(),
            name: "User A".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    let agent_service = AgentService::new(
        SurrealAgentRepo::new(db.clone()),
        &CacheConfig::default(),
        test_resource_manager(),
        test_policy_service(&db).await,
        user_service,
    );

    agent_service
        .clone_all_builtins_for_user("user-a", &storage)
        .await
        .unwrap();
    // Idempotent: second call should not duplicate or error.
    agent_service
        .clone_all_builtins_for_user("user-a", &storage)
        .await
        .unwrap();

    let agents = agent_service.list("user-a").await.unwrap();
    let handles: Vec<&str> = agents.iter().map(|a| a.handle.as_ref()).collect();
    assert!(
        handles.contains(&"developer"),
        "Expected developer handle, got: {handles:?}"
    );
    assert!(
        handles.contains(&"system"),
        "Expected system handle, got: {handles:?}"
    );
    // All cloned rows belong to the user (no user_id IS NONE rows surface).
    for agent in &agents {
        assert_eq!(agent.user_id, "user-a");
    }
}

#[tokio::test]
async fn agent_service_find_by_id_caches() {
    let db = test_db().await;
    let svc = AgentService::new(
        SurrealAgentRepo::new(db.clone()),
        &CacheConfig::default(),
        test_resource_manager(),
        test_policy_service(&db).await,
        test_user_service(&db),
    );
    let repo = SurrealAgentRepo::new(db);
    let agent = test_agent("user-1");
    repo.create(&agent).await.unwrap();

    let first = svc.find_by_id(&agent.id).await.unwrap().unwrap();
    let second = svc.find_by_id(&agent.id).await.unwrap().unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.name, second.name);
}

#[tokio::test]
async fn agent_service_update_invalidates_cache() {
    use frona::agent::models::UpdateAgentRequest;

    let db = test_db().await;
    let svc = AgentService::new(
        SurrealAgentRepo::new(db.clone()),
        &CacheConfig::default(),
        test_resource_manager(),
        test_policy_service(&db).await,
        test_user_service(&db),
    );
    let repo = SurrealAgentRepo::new(db);
    let agent = test_agent("user-1");
    repo.create(&agent).await.unwrap();

    let cached = svc.find_by_id(&agent.id).await.unwrap().unwrap();
    assert_eq!(cached.name, "Test Agent");

    svc.update(
        "user-1",
        &agent.id,
        UpdateAgentRequest {
            name: Some("Renamed".to_string()),
            description: None,
            model_group: None,
            enabled: None,
            tools: None,
            skills: None,
            sandbox_policy: None,
            sandbox_limits: None,
            prompt: None,
            identity: None,
        },
    )
    .await
    .unwrap();

    let after = svc.find_by_id(&agent.id).await.unwrap().unwrap();
    assert_eq!(after.name, "Renamed");
}

#[tokio::test]
async fn agent_service_delete_invalidates_cache() {
    let db = test_db().await;
    let user_service = test_user_service(&db);
    seed_user(&user_service, "user-1").await;
    let svc = AgentService::new(
        SurrealAgentRepo::new(db.clone()),
        &CacheConfig::default(),
        test_resource_manager(),
        test_policy_service(&db).await,
        user_service,
    );
    let repo = SurrealAgentRepo::new(db);
    let agent = test_agent("user-1");
    repo.create(&agent).await.unwrap();

    assert!(svc.find_by_id(&agent.id).await.unwrap().is_some());

    svc.delete("user-1", &agent.id).await.unwrap();

    assert!(svc.find_by_id(&agent.id).await.unwrap().is_none());
}

mod helpers;
