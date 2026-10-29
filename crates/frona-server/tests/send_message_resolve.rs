use chrono::Utc;
use frona::agent::models::Agent;
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::chat::models::Chat;
use frona::core::repository::Repository;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_chat(id: &str, user_id: &str, agent_id: &str, task_id: Option<&str>) -> Chat {
    let now = Utc::now();
    Chat {
        agent_context_state: Default::default(),
        id: id.to_string(),
        user_id: user_id.to_string(),
        space_id: None,
        task_id: task_id.map(|s| s.to_string()),
        agent_id: agent_id.to_string(),
        title: Some(format!("Chat {id}")),
        archived_at: None,
        channel_id: None,
        channel_external_id: None,
        metadata: Default::default(),
        created_at: now,
        updated_at: now,
    }
}

fn test_task(id: &str, user_id: &str, agent_id: &str, kind: TaskKind) -> Task {
    let now = Utc::now();
    Task {
        id: id.to_string(),
        user_id: user_id.to_string(),
        agent_id: agent_id.to_string(),
        space_id: None,
        chat_id: None,
        title: "Test task".to_string(),
        description: String::new(),
        status: TaskStatus::InProgress,
        kind,
        run_at: None,
        result_summary: None,
        error_message: None,
        quarantined: false,
        result_schema: None,
        result_description: None,
        created_at: now,
        updated_at: now,
    }
}

fn test_agent(id: &str, user_id: &str, heartbeat_chat_id: Option<&str>) -> Agent {
    let now = Utc::now();
    Agent {
        id: id.to_string(),
        user_id: user_id.to_string(),
        handle: frona::core::Handle::try_new(id).unwrap(),
        name: format!("Agent {id}"),
        description: String::new(),
        model_group: "primary".to_string(),
        enabled: true,
        skills: None,
        sandbox_limits: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: Default::default(),
        prompt: None,
        heartbeat_interval: None,
        next_heartbeat_at: None,
        heartbeat_chat_id: heartbeat_chat_id.map(|s| s.to_string()),
        created_at: now,
        updated_at: now,
    }
}

/// Delegation chain: User → Agent A (chat-1) → Task T1 → Agent B (chat-2) → Task T2 → Agent C (chat-3)
/// TaskKind::Delegation carries source_chat_id linking back through the chain.
/// Walking from chat-3 should resolve to chat-1 (the user-facing origin).
#[tokio::test]
async fn test_task_chain_walk_resolves_to_origin() {
    let db = test_db().await;
    let chat_repo: SurrealRepo<Chat> = SurrealRepo::new(db.clone());
    let task_repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());

    let user_id = "user-1";

    // Chat 1: user-facing (no task_id)
    let chat1 = test_chat("chat-1", user_id, "agent-a", None);
    chat_repo.create(&chat1).await.unwrap();

    // Task T1: delegation from chat-1 to agent-b
    let t1 = test_task(
        "task-1",
        user_id,
        "agent-b",
        TaskKind::Delegation {
            source_agent_id: "agent-a".to_string(),
            source_chat_id: "chat-1".to_string(),
            resume_parent: true,
        },
    );
    task_repo.create(&t1).await.unwrap();

    // Chat 2: task chat for T1
    let chat2 = test_chat("chat-2", user_id, "agent-b", Some("task-1"));
    chat_repo.create(&chat2).await.unwrap();

    // Task T2: delegation from chat-2 to agent-c
    let t2 = test_task(
        "task-2",
        user_id,
        "agent-c",
        TaskKind::Delegation {
            source_agent_id: "agent-b".to_string(),
            source_chat_id: "chat-2".to_string(),
            resume_parent: true,
        },
    );
    task_repo.create(&t2).await.unwrap();

    // Chat 3: task chat for T2
    let chat3 = test_chat("chat-3", user_id, "agent-c", Some("task-2"));
    chat_repo.create(&chat3).await.unwrap();

    let chat_loaded = chat_repo.find_by_id("chat-3").await.unwrap().unwrap();
    assert_eq!(chat_loaded.task_id.as_deref(), Some("task-2"));

    let task_loaded = task_repo.find_by_id("task-2").await.unwrap().unwrap();
    assert_eq!(task_loaded.kind.source_chat_id(), Some("chat-2"));

    let chat2_loaded = chat_repo.find_by_id("chat-2").await.unwrap().unwrap();
    assert_eq!(chat2_loaded.task_id.as_deref(), Some("task-1"));

    let task1_loaded = task_repo.find_by_id("task-1").await.unwrap().unwrap();
    assert_eq!(task1_loaded.kind.source_chat_id(), Some("chat-1"));

    let chat1_loaded = chat_repo.find_by_id("chat-1").await.unwrap().unwrap();
    assert!(chat1_loaded.task_id.is_none());
}

/// Heartbeat chats should be excluded from standalone chat lists.
#[tokio::test]
async fn test_heartbeat_chat_excluded_from_standalone() {
    let db = test_db().await;
    let chat_repo: SurrealRepo<Chat> = SurrealRepo::new(db.clone());
    let agent_repo: SurrealRepo<Agent> = SurrealRepo::new(db.clone());

    let user_id = "user-1";

    let regular = test_chat("chat-regular", user_id, "agent-a", None);
    chat_repo.create(&regular).await.unwrap();

    let heartbeat = test_chat("chat-heartbeat", user_id, "agent-a", None);
    chat_repo.create(&heartbeat).await.unwrap();

    // Agent with heartbeat_chat_id
    let agent = test_agent("agent-a", user_id, Some("chat-heartbeat"));
    agent_repo.create(&agent).await.unwrap();

    // Standalone chats include both (repo doesn't know about heartbeats)
    use frona::chat::repository::ChatRepository;
    let standalone = chat_repo.find_standalone_by_user_id(user_id).await.unwrap();
    assert_eq!(standalone.len(), 2);

    // But filtering by heartbeat IDs should exclude the heartbeat chat
    let agent_loaded: Agent = agent_repo.find_by_id("agent-a").await.unwrap().unwrap();
    let heartbeat_ids: Vec<String> = agent_loaded.heartbeat_chat_id.into_iter().collect();

    let filtered: Vec<_> = standalone
        .into_iter()
        .filter(|c| !heartbeat_ids.contains(&c.id))
        .collect();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, "chat-regular");
}

/// TaskKind::source_chat_id() returns the correct value for each variant.
#[test]
fn test_task_kind_source_chat_id() {
    assert_eq!(
        TaskKind::Direct {
            source_chat_id: None
        }
        .source_chat_id(),
        None
    );

    let delegation = TaskKind::Delegation {
        source_agent_id: "a".into(),
        source_chat_id: "chat-1".into(),
        resume_parent: true,
    };
    assert_eq!(delegation.source_chat_id(), Some("chat-1"));

    let cron_with = TaskKind::Cron {
        cron_expression: "* * * * *".into(),
        timezone: None,
        next_run_at: None,
        source_agent_id: None,
        source_chat_id: Some("chat-2".into()),
        mode: Default::default(),
        concurrency: Default::default(),
        process_result: false,
    };
    assert_eq!(cron_with.source_chat_id(), Some("chat-2"));

    let cron_without = TaskKind::Cron {
        cron_expression: "* * * * *".into(),
        timezone: None,
        next_run_at: None,
        source_agent_id: None,
        source_chat_id: None,
        mode: Default::default(),
        concurrency: Default::default(),
        process_result: false,
    };
    assert_eq!(cron_without.source_chat_id(), None);
}
