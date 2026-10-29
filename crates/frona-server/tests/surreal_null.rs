use chrono::Utc;
use frona::chat::models::Chat;
use frona::chat::repository::ChatRepository;
use frona::core::repository::Repository;
use frona::db::init as db;
use frona::db::repo::chats::SurrealChatRepo;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_chat(user_id: &str, space_id: Option<&str>, title: Option<&str>) -> Chat {
    let now = Utc::now();
    Chat {
        agent_context_state: Default::default(),
        id: frona::core::repository::new_id(),
        user_id: user_id.to_string(),
        space_id: space_id.map(|s| s.to_string()),
        task_id: None,
        agent_id: "some-agent".to_string(),
        title: title.map(|s| s.to_string()),
        archived_at: None,
        channel_id: None,
        channel_external_id: None,
        metadata: Default::default(),
        created_at: now,
        updated_at: now,
    }
}

// Agent user_id is required post-refactor; previous tests covering the
// `user_id IS NONE` case are obsolete (the new schema enforces ownership at
// clone-on-signup).

#[tokio::test]
async fn test_chat_none_optional_fields_round_trip() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let chat = test_chat("user-1", None, None);
    repo.create(&chat).await.unwrap();

    let found = repo.find_by_id(&chat.id).await.unwrap().unwrap();
    assert_eq!(found.space_id, None);
    assert_eq!(found.title, None);

    let standalone = repo.find_standalone_by_user_id("user-1").await.unwrap();
    assert!(
        standalone.iter().any(|c| c.id == chat.id),
        "chat with space_id=None should appear in find_standalone_by_user_id"
    );
}

#[tokio::test]
async fn test_chat_with_space_id_excluded_from_standalone() {
    let db = test_db().await;
    let repo = SurrealChatRepo::new(db);

    let standalone_chat = test_chat("user-1", None, Some("Standalone"));
    let space_chat = test_chat("user-1", Some("space-1"), Some("In Space"));
    repo.create(&standalone_chat).await.unwrap();
    repo.create(&space_chat).await.unwrap();

    let standalone = repo.find_standalone_by_user_id("user-1").await.unwrap();
    assert_eq!(standalone.len(), 1);
    assert_eq!(standalone[0].id, standalone_chat.id);
}
