use chrono::Utc;
use frona::agent::models::Agent;
use frona::agent::repository::AgentRepository;
use frona::core::repository::Repository;
use frona::db::init as db;
use frona::db::repo::agents::SurrealAgentRepo;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_agent(user_id: &str) -> Agent {
    let now = Utc::now();
    Agent {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: Some(user_id.to_string()),
        name: "Test Agent".to_string(),
        description: "A test agent".to_string(),
        model_group: "primary".to_string(),
        enabled: true,
        tools: vec!["browser".to_string()],
        sandbox_config: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: std::collections::BTreeMap::new(),
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
    assert!(agents.iter().all(|a| a.user_id.as_deref() == Some("user-1")));

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
async fn test_seed_config_agents_visible_in_find_by_user_id() {
    use frona::agent::workspace::AgentWorkspaceManager;

    let db = test_db().await;
    let shared_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("resources");
    let workspaces = AgentWorkspaceManager::new("/tmp/frona_test_seed_visible", shared_dir.join("agents"));

    db::seed_config_agents(&db, &workspaces).await.unwrap();

    let repo = SurrealAgentRepo::new(db);
    let agents = repo.find_by_user_id("any-user").await.unwrap();
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();

    assert!(names.contains(&"tester"), "Seeded agents should include 'tester', got: {names:?}");
    assert!(names.contains(&"developer"), "Seeded agents should include 'developer', got: {names:?}");
    assert!(names.contains(&"researcher"), "Seeded agents should include 'researcher', got: {names:?}");
    assert!(names.contains(&"system"), "Seeded agents should include 'system', got: {names:?}");
}
