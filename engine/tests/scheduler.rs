use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use frona::agent::models::Agent;
use frona::agent::repository::AgentRepository;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::insights::SurrealInsightRepo;
use frona::memory::insight::models::Insight;
use frona::memory::insight::repository::InsightRepository;
use frona::core::repository::Repository;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn make_insight(agent_id: &str, content: &str) -> Insight {
    Insight {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        user_id: None,
        content: content.to_string(),
        source_chat_id: None,
        created_at: Utc::now(),
    }
}

fn make_user_insight(user_id: &str, content: &str) -> Insight {
    Insight {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: String::new(),
        user_id: Some(user_id.to_string()),
        content: content.to_string(),
        source_chat_id: None,
        created_at: Utc::now(),
    }
}

fn make_agent(id: &str, user_id: &str, heartbeat_interval: Option<u64>, next_heartbeat_at: Option<chrono::DateTime<Utc>>) -> Agent {
    let now = Utc::now();
    Agent {
        id: id.to_string(),
        user_id: Some(user_id.to_string()),
        name: format!("Agent {id}"),
        description: "test agent".to_string(),
        model_group: "primary".to_string(),
        enabled: true,
        tools: vec![],
        sandbox_config: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: BTreeMap::new(),
        heartbeat_interval,
        next_heartbeat_at,
        heartbeat_chat_id: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn insight_compaction_discovers_distinct_agent_ids() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    repo.create(&make_insight("agent-a", "fact 1")).await.unwrap();
    repo.create(&make_insight("agent-b", "fact 2")).await.unwrap();
    repo.create(&make_insight("agent-a", "fact 3")).await.unwrap();

    let mut ids = repo.find_distinct_agent_ids().await.unwrap();
    ids.sort();
    assert_eq!(ids, vec!["agent-a", "agent-b"]);
}

#[tokio::test]
async fn insight_compaction_discovers_distinct_user_ids() {
    let db = test_db().await;
    let repo: SurrealInsightRepo = SurrealRepo::new(db);

    repo.create(&make_user_insight("user-x", "pref 1")).await.unwrap();
    repo.create(&make_user_insight("user-y", "pref 2")).await.unwrap();
    repo.create(&make_user_insight("user-x", "pref 3")).await.unwrap();

    let mut ids = repo.find_distinct_user_ids().await.unwrap();
    ids.sort();
    assert_eq!(ids, vec!["user-x", "user-y"]);
}

#[tokio::test]
async fn heartbeat_due_discovery_returns_agents_with_past_heartbeat() {
    let db = test_db().await;
    let repo: SurrealRepo<Agent> = SurrealRepo::new(db);

    let now = Utc::now();
    let past = now - Duration::minutes(10);
    let future = now + Duration::minutes(10);

    let due_agent = make_agent("agent-due", "user-1", Some(30), Some(past));
    let not_due_agent = make_agent("agent-future", "user-1", Some(30), Some(future));
    let no_heartbeat = make_agent("agent-none", "user-1", None, None);

    repo.create(&due_agent).await.unwrap();
    repo.create(&not_due_agent).await.unwrap();
    repo.create(&no_heartbeat).await.unwrap();

    let due = repo.find_due_heartbeats(now).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "agent-due");
}

#[tokio::test]
async fn heartbeat_due_excludes_disabled_agents() {
    let db = test_db().await;
    let repo: SurrealRepo<Agent> = SurrealRepo::new(db);

    let now = Utc::now();
    let past = now - Duration::minutes(10);

    let mut disabled = make_agent("agent-disabled", "user-1", Some(30), Some(past));
    disabled.enabled = false;

    repo.create(&disabled).await.unwrap();

    let due = repo.find_due_heartbeats(now).await.unwrap();
    assert!(due.is_empty());
}
