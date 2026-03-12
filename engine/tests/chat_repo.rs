use std::collections::HashMap;

use chrono::Utc;
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::db::init as db;
use frona::db::repo::chats::SurrealChatRepo;
use frona::db::repo::messages::SurrealMessageRepo;
use frona::api::files::Attachment;
use frona::chat::message::models::{Message, MessageRole};
use frona::chat::message::repository::MessageRepository;
use frona::chat::models::Chat;
use frona::chat::repository::ChatRepository;
use frona::core::repository::Repository;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_chat(user_id: &str, agent_id: &str, task_id: Option<&str>) -> Chat {
    let now = Utc::now();
    Chat {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        space_id: None,
        task_id: task_id.map(|s| s.to_string()),
        agent_id: agent_id.to_string(),
        title: Some("Test".to_string()),
        archived_at: None,
        created_at: now,
        updated_at: now,
    }
}

fn test_chat_in_space(user_id: &str, space_id: &str) -> Chat {
    let now = Utc::now();
    Chat {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        space_id: Some(space_id.to_string()),
        task_id: None,
        agent_id: "system".to_string(),
        title: Some("Space chat".to_string()),
        archived_at: None,
        created_at: now,
        updated_at: now,
    }
}

fn test_message(chat_id: &str, content: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        chat_id: chat_id.to_string(),
        role: MessageRole::User,
        content: content.to_string(),
        agent_id: None,
        tool_calls: None,
        tool_call_id: None,
        tool: None,
        attachments: vec![],
        contact_id: None,
        system_prompt: None,
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn test_standalone_excludes_task_chats() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let regular = test_chat("user-1", "system", None);
    let task_chat = test_chat("user-1", "system", Some("task-123"));

    repo.create(&regular).await.unwrap();
    repo.create(&task_chat).await.unwrap();

    let standalone = repo.find_standalone_by_user_id("user-1").await.unwrap();
    assert_eq!(standalone.len(), 1);
    assert_eq!(standalone[0].id, regular.id);
}

#[tokio::test]
async fn test_task_chat_round_trips() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let chat = test_chat("user-1", "system", Some("task-456"));
    repo.create(&chat).await.unwrap();

    let found = repo.find_by_id(&chat.id).await.unwrap().unwrap();
    assert_eq!(found.task_id.as_deref(), Some("task-456"));
}

#[tokio::test]
async fn test_chat_count_grouped_by_agent() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db.clone());

    repo.create(&test_chat("user-1", "agent-a", None)).await.unwrap();
    repo.create(&test_chat("user-1", "agent-a", None)).await.unwrap();
    repo.create(&test_chat("user-1", "agent-a", None)).await.unwrap();
    repo.create(&test_chat("user-1", "agent-b", None)).await.unwrap();
    repo.create(&test_chat("user-2", "agent-a", None)).await.unwrap();

    let count_map: HashMap<String, u64> = db
        .query("SELECT agent_id, count() AS count FROM chat WHERE user_id = $user_id GROUP BY agent_id")
        .bind(("user_id", "user-1".to_string()))
        .await
        .and_then(|mut r| r.take::<Vec<serde_json::Value>>(0))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let agent_id = v.get("agent_id")?.as_str()?.to_string();
            let count = v.get("count")?.as_u64()?;
            Some((agent_id, count))
        })
        .collect();

    assert_eq!(count_map.get("agent-a"), Some(&3));
    assert_eq!(count_map.get("agent-b"), Some(&1));
    assert_eq!(count_map.get("agent-c"), None);
}

#[tokio::test]
async fn test_chat_count_empty_for_no_chats() {
    let db = test_db().await;

    let count_map: HashMap<String, u64> = db
        .query("SELECT agent_id, count() AS count FROM chat WHERE user_id = $user_id GROUP BY agent_id")
        .bind(("user_id", "user-no-chats".to_string()))
        .await
        .and_then(|mut r| r.take::<Vec<serde_json::Value>>(0))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let agent_id = v.get("agent_id")?.as_str()?.to_string();
            let count = v.get("count")?.as_u64()?;
            Some((agent_id, count))
        })
        .collect();

    assert!(count_map.is_empty());
}

// ---------------------------------------------------------------------------
// Archive / unarchive integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_archived_chat_excluded_from_find_by_user_id() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let active = test_chat("user-1", "system", None);
    let mut archived = test_chat("user-1", "system", None);
    archived.archived_at = Some(Utc::now());

    repo.create(&active).await.unwrap();
    repo.create(&archived).await.unwrap();

    let results = repo.find_by_user_id("user-1").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, active.id);
}

#[tokio::test]
async fn test_archived_chat_excluded_from_find_standalone_by_user_id() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let active = test_chat("user-1", "system", None);
    let mut archived = test_chat("user-1", "system", None);
    archived.archived_at = Some(Utc::now());

    repo.create(&active).await.unwrap();
    repo.create(&archived).await.unwrap();

    let results = repo.find_standalone_by_user_id("user-1").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, active.id);
}

#[tokio::test]
async fn test_archived_chat_excluded_from_find_by_space_id() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let active = test_chat_in_space("user-1", "space-1");
    let mut archived = test_chat_in_space("user-1", "space-1");
    archived.archived_at = Some(Utc::now());

    repo.create(&active).await.unwrap();
    repo.create(&archived).await.unwrap();

    let results = repo.find_by_space_id("space-1").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, active.id);
}

#[tokio::test]
async fn test_find_archived_by_user_id_returns_only_archived() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let active = test_chat("user-1", "system", None);
    let mut archived = test_chat("user-1", "system", None);
    archived.archived_at = Some(Utc::now());

    repo.create(&active).await.unwrap();
    repo.create(&archived).await.unwrap();

    let results = repo.find_archived_by_user_id("user-1").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, archived.id);
    assert!(results[0].archived_at.is_some());
}

#[tokio::test]
async fn test_find_archived_by_user_id_scoped_to_user() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let mut archived_u1 = test_chat("user-1", "system", None);
    archived_u1.archived_at = Some(Utc::now());
    let mut archived_u2 = test_chat("user-2", "system", None);
    archived_u2.archived_at = Some(Utc::now());

    repo.create(&archived_u1).await.unwrap();
    repo.create(&archived_u2).await.unwrap();

    let results = repo.find_archived_by_user_id("user-1").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, archived_u1.id);
}

#[tokio::test]
async fn test_find_archived_returns_empty_when_none_archived() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    repo.create(&test_chat("user-1", "system", None)).await.unwrap();

    let results = repo.find_archived_by_user_id("user-1").await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_archived_at_round_trips_through_repo() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let mut chat = test_chat("user-1", "system", None);
    repo.create(&chat).await.unwrap();

    let found = repo.find_by_id(&chat.id).await.unwrap().unwrap();
    assert!(found.archived_at.is_none());

    chat.archived_at = Some(Utc::now());
    repo.update(&chat).await.unwrap();

    let found = repo.find_by_id(&chat.id).await.unwrap().unwrap();
    assert!(found.archived_at.is_some());

    chat.archived_at = None;
    repo.update(&chat).await.unwrap();

    let found = repo.find_by_id(&chat.id).await.unwrap().unwrap();
    assert!(found.archived_at.is_none());
}

// ---------------------------------------------------------------------------
// Message delete_by_chat_id integration test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_by_chat_id_removes_all_messages() {
    let db = test_db().await;
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let chat_repo = SurrealChatRepo::new(db);

    let chat = test_chat("user-1", "system", None);
    chat_repo.create(&chat).await.unwrap();

    let m1 = test_message(&chat.id, "hello");
    let m2 = test_message(&chat.id, "world");
    msg_repo.create(&m1).await.unwrap();
    msg_repo.create(&m2).await.unwrap();

    let msgs = msg_repo.find_by_chat_id(&chat.id).await.unwrap();
    assert_eq!(msgs.len(), 2);

    msg_repo.delete_by_chat_id(&chat.id).await.unwrap();

    let msgs = msg_repo.find_by_chat_id(&chat.id).await.unwrap();
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn test_delete_by_chat_id_does_not_affect_other_chats() {
    let db = test_db().await;
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let chat_repo = SurrealChatRepo::new(db);

    let chat_a = test_chat("user-1", "system", None);
    let chat_b = test_chat("user-1", "system", None);
    chat_repo.create(&chat_a).await.unwrap();
    chat_repo.create(&chat_b).await.unwrap();

    msg_repo.create(&test_message(&chat_a.id, "a1")).await.unwrap();
    msg_repo.create(&test_message(&chat_b.id, "b1")).await.unwrap();

    msg_repo.delete_by_chat_id(&chat_a.id).await.unwrap();

    let msgs_a = msg_repo.find_by_chat_id(&chat_a.id).await.unwrap();
    let msgs_b = msg_repo.find_by_chat_id(&chat_b.id).await.unwrap();
    assert!(msgs_a.is_empty());
    assert_eq!(msgs_b.len(), 1);
}

// ---------------------------------------------------------------------------
// Cascade delete integration tests
// ---------------------------------------------------------------------------

fn test_task(user_id: &str, agent_id: &str, chat_id: Option<&str>) -> Task {
    let now = Utc::now();
    Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        agent_id: agent_id.to_string(),
        space_id: None,
        chat_id: chat_id.map(|s| s.to_string()),
        title: "Test task".to_string(),
        description: "Test description".to_string(),
        status: TaskStatus::Completed,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn test_cascade_delete_chat_removes_messages() {
    let db = test_db().await;
    let chat_repo = SurrealChatRepo::new(db.clone());
    let msg_repo = SurrealMessageRepo::new(db);

    let chat = test_chat("user-1", "system", None);
    chat_repo.create(&chat).await.unwrap();

    msg_repo.create(&test_message(&chat.id, "hello")).await.unwrap();
    msg_repo.create(&test_message(&chat.id, "world")).await.unwrap();
    assert_eq!(msg_repo.find_by_chat_id(&chat.id).await.unwrap().len(), 2);

    chat_repo.delete(&chat.id).await.unwrap();

    assert!(msg_repo.find_by_chat_id(&chat.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_cascade_delete_task_removes_chat_and_messages() {
    let db = test_db().await;
    let chat_repo = SurrealChatRepo::new(db.clone());
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let task_repo = frona::db::repo::tasks::SurrealTaskRepo::new(db);

    let chat = test_chat("user-1", "system", Some("task-1"));
    chat_repo.create(&chat).await.unwrap();

    msg_repo.create(&test_message(&chat.id, "msg1")).await.unwrap();
    msg_repo.create(&test_message(&chat.id, "msg2")).await.unwrap();

    let task = test_task("user-1", "system", Some(&chat.id));
    task_repo.create(&task).await.unwrap();

    task_repo.delete(&task.id).await.unwrap();

    assert!(chat_repo.find_by_id(&chat.id).await.unwrap().is_none());
    assert!(msg_repo.find_by_chat_id(&chat.id).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Attachment integration tests
// ---------------------------------------------------------------------------

fn test_attachment(filename: &str, owner: &str, path: &str) -> Attachment {
    Attachment {
        filename: filename.to_string(),
        content_type: "text/plain".to_string(),
        size_bytes: 100,
        owner: owner.to_string(),
        path: path.to_string(),
        url: None,
    }
}

#[tokio::test]
async fn test_message_with_attachments_round_trips_through_db() {
    let db = test_db().await;
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let chat_repo = SurrealChatRepo::new(db);

    let chat = test_chat("user-1", "system", None);
    chat_repo.create(&chat).await.unwrap();

    let mut msg = test_message(&chat.id, "check this file");
    msg.attachments = vec![test_attachment("report.pdf", "user:uid", "report.pdf")];

    msg_repo.create(&msg).await.unwrap();

    let found = msg_repo.find_by_chat_id(&chat.id).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].attachments.len(), 1);
    assert_eq!(found[0].attachments[0].filename, "report.pdf");
    assert_eq!(found[0].attachments[0].owner, "user:uid");
    assert_eq!(found[0].attachments[0].path, "report.pdf");
}

#[tokio::test]
async fn test_message_without_attachments_backward_compat() {
    let db = test_db().await;
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let chat_repo = SurrealChatRepo::new(db);

    let chat = test_chat("user-1", "system", None);
    chat_repo.create(&chat).await.unwrap();

    let msg = test_message(&chat.id, "no attachments");
    msg_repo.create(&msg).await.unwrap();

    let found = msg_repo.find_by_chat_id(&chat.id).await.unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].attachments.is_empty());
}

#[tokio::test]
async fn test_find_attachments_by_chat_id_flat_list() {
    let db = test_db().await;
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let chat_repo = SurrealChatRepo::new(db);

    let chat = test_chat("user-1", "system", None);
    chat_repo.create(&chat).await.unwrap();

    let mut m1 = test_message(&chat.id, "first");
    m1.attachments = vec![test_attachment("a.txt", "user:uid", "a.txt")];

    let mut m2 = test_message(&chat.id, "second");
    m2.attachments = vec![
        test_attachment("b.txt", "agent:dev", "b.txt"),
        test_attachment("c.csv", "agent:dev", "c.csv"),
    ];

    msg_repo.create(&m1).await.unwrap();
    msg_repo.create(&m2).await.unwrap();

    let attachments = msg_repo.find_attachments_by_chat_id(&chat.id).await.unwrap();
    assert_eq!(attachments.len(), 3);

    let filenames: Vec<&str> = attachments.iter().map(|a| a.filename.as_str()).collect();
    assert!(filenames.contains(&"a.txt"));
    assert!(filenames.contains(&"b.txt"));
    assert!(filenames.contains(&"c.csv"));
}

#[tokio::test]
async fn test_find_attachments_by_chat_id_empty() {
    let db = test_db().await;
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let chat_repo = SurrealChatRepo::new(db);

    let chat = test_chat("user-1", "system", None);
    chat_repo.create(&chat).await.unwrap();

    let msg = test_message(&chat.id, "no files");
    msg_repo.create(&msg).await.unwrap();

    let attachments = msg_repo.find_attachments_by_chat_id(&chat.id).await.unwrap();
    assert!(attachments.is_empty());
}

#[tokio::test]
async fn test_find_attachments_scoped_to_chat() {
    let db = test_db().await;
    let msg_repo = SurrealMessageRepo::new(db.clone());
    let chat_repo = SurrealChatRepo::new(db);

    let chat_a = test_chat("user-1", "system", None);
    let chat_b = test_chat("user-1", "system", None);
    chat_repo.create(&chat_a).await.unwrap();
    chat_repo.create(&chat_b).await.unwrap();

    let mut m_a = test_message(&chat_a.id, "in chat a");
    m_a.attachments = vec![test_attachment("a.txt", "user:uid", "a.txt")];

    let mut m_b = test_message(&chat_b.id, "in chat b");
    m_b.attachments = vec![test_attachment("b.txt", "user:uid", "b.txt")];

    msg_repo.create(&m_a).await.unwrap();
    msg_repo.create(&m_b).await.unwrap();

    let attachments = msg_repo.find_attachments_by_chat_id(&chat_a.id).await.unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename, "a.txt");
}
