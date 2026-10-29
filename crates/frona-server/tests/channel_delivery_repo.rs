use chrono::{Duration, Utc};
use frona::chat::message::models::{
    DeliveryState, Message, MessageDelivery, MessageRole, MessageStatus,
};
use frona::chat::message::repository::MessageRepository;
use frona::chat::models::Chat;
use frona::core::repository::Repository;
use frona::db::init as db;
use frona::db::repo::chats::SurrealChatRepo;
use frona::db::repo::messages::SurrealMessageRepo;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn channel_chat(channel_id: &str, ext_id: &str) -> Chat {
    let now = Utc::now();
    Chat {
        agent_context_state: Default::default(),
        id: frona::core::repository::new_id(),
        user_id: "u1".into(),
        space_id: Some("s1".into()),
        task_id: None,
        agent_id: "agent-1".into(),
        title: None,
        archived_at: None,
        channel_id: Some(channel_id.into()),
        channel_external_id: Some(ext_id.into()),
        metadata: Default::default(),
        created_at: now,
        updated_at: now,
    }
}

fn agent_msg(chat_id: &str, delivery: Option<MessageDelivery>) -> Message {
    let mut msg = Message::builder(chat_id, MessageRole::Agent, "hi".into())
        .agent_id("agent-1".into())
        .status(MessageStatus::Completed)
        .build();
    msg.delivery = delivery;
    msg
}

#[tokio::test]
async fn find_due_deliveries_returns_only_eligible_rows() {
    let db = test_db().await;
    let chat_repo = SurrealChatRepo::new(db.clone());
    let msg_repo = SurrealMessageRepo::new(db.clone());

    let chat = channel_chat("channel:tg", "dm:1");
    chat_repo.create(&chat).await.unwrap();

    let now = Utc::now();
    let past = now - Duration::seconds(1);
    let future = now + Duration::hours(1);

    let eligible_pending = {
        let d = MessageDelivery::pending(past);
        agent_msg(&chat.id, Some(d))
    };
    msg_repo.create(&eligible_pending).await.unwrap();

    let mut eligible_failed = agent_msg(&chat.id, None);
    eligible_failed.delivery = Some(MessageDelivery {
        state: DeliveryState::Failed,
        attempts: 2,
        next_attempt_at: Some(past),
        last_attempt_at: Some(past),
        last_error: Some("transient".into()),
        failure_kind: None,
        sent_at: None,
        delivered_at: None,
        tool_index: 0,
    });
    msg_repo.create(&eligible_failed).await.unwrap();

    let backed_off = agent_msg(&chat.id, Some(MessageDelivery::pending(future)));
    msg_repo.create(&backed_off).await.unwrap();

    let mut terminal = agent_msg(&chat.id, None);
    terminal.delivery = Some(MessageDelivery {
        state: DeliveryState::Failed,
        attempts: 5,
        next_attempt_at: None,
        last_attempt_at: Some(past),
        last_error: Some("forbidden".into()),
        failure_kind: None,
        sent_at: None,
        delivered_at: None,
        tool_index: 0,
    });
    msg_repo.create(&terminal).await.unwrap();

    let mut sent = agent_msg(&chat.id, None);
    sent.delivery = Some(MessageDelivery {
        state: DeliveryState::Sent,
        attempts: 1,
        next_attempt_at: None,
        last_attempt_at: Some(past),
        last_error: None,
        failure_kind: None,
        sent_at: Some(past),
        delivered_at: None,
        tool_index: 0,
    });
    msg_repo.create(&sent).await.unwrap();

    let non_channel = agent_msg(&chat.id, None);
    msg_repo.create(&non_channel).await.unwrap();

    let due = msg_repo
        .find_due_deliveries(now, 50)
        .await
        .expect("query should run");
    let due_ids: Vec<&str> = due.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        due_ids.len(),
        2,
        "exactly the two eligible rows should be returned, got {due_ids:?}",
    );
    assert!(due_ids.contains(&eligible_pending.id.as_str()));
    assert!(due_ids.contains(&eligible_failed.id.as_str()));
}

#[tokio::test]
async fn resume_deliveries_pulls_backed_off_rows_forward() {
    let db = test_db().await;
    let chat_repo = SurrealChatRepo::new(db.clone());
    let msg_repo = SurrealMessageRepo::new(db.clone());

    let chat_a = channel_chat("channel:tg-a", "dm:1");
    let chat_b = channel_chat("channel:tg-b", "dm:2");
    chat_repo.create(&chat_a).await.unwrap();
    chat_repo.create(&chat_b).await.unwrap();

    let now = Utc::now();
    let future = now + Duration::hours(1);

    let backed_off_a1 = agent_msg(&chat_a.id, Some(MessageDelivery::pending(future)));
    let backed_off_a2 = agent_msg(&chat_a.id, Some(MessageDelivery::pending(future)));
    msg_repo.create(&backed_off_a1).await.unwrap();
    msg_repo.create(&backed_off_a2).await.unwrap();

    let backed_off_b = agent_msg(&chat_b.id, Some(MessageDelivery::pending(future)));
    msg_repo.create(&backed_off_b).await.unwrap();

    let mut terminal_a = agent_msg(&chat_a.id, None);
    terminal_a.delivery = Some(MessageDelivery {
        state: DeliveryState::Failed,
        attempts: 5,
        next_attempt_at: None,
        last_attempt_at: Some(now),
        last_error: Some("done".into()),
        failure_kind: None,
        sent_at: None,
        delivered_at: None,
        tool_index: 0,
    });
    msg_repo.create(&terminal_a).await.unwrap();

    let updated = msg_repo
        .resume_deliveries_for_channel("channel:tg-a", now)
        .await
        .unwrap();
    assert_eq!(updated, 2, "exactly the two backed-off rows in channel A");

    let due = msg_repo.find_due_deliveries(now, 50).await.unwrap();
    let due_ids: Vec<&str> = due.iter().map(|m| m.id.as_str()).collect();
    assert!(due_ids.contains(&backed_off_a1.id.as_str()));
    assert!(due_ids.contains(&backed_off_a2.id.as_str()));
    assert!(
        !due_ids.contains(&backed_off_b.id.as_str()),
        "channel B's backed-off row must not be resumed by channel A"
    );
    assert!(
        !due_ids.contains(&terminal_a.id.as_str()),
        "terminal Failed rows must not be swept"
    );
}

fn executing_agent_msg(chat_id: &str, delivery: Option<MessageDelivery>) -> Message {
    let mut msg = Message::builder(chat_id, MessageRole::Agent, "hi".into())
        .agent_id("agent-1".into())
        .status(MessageStatus::Executing)
        .build();
    msg.delivery = delivery;
    msg
}

#[tokio::test]
async fn find_due_deliveries_excludes_executing_messages() {
    let db = test_db().await;
    let chat_repo = SurrealChatRepo::new(db.clone());
    let msg_repo = SurrealMessageRepo::new(db.clone());

    let chat = channel_chat("channel:tg", "dm:1");
    chat_repo.create(&chat).await.unwrap();

    let now = Utc::now();
    let past = now - Duration::seconds(1);

    let executing = executing_agent_msg(&chat.id, Some(MessageDelivery::pending(past)));
    msg_repo.create(&executing).await.unwrap();

    let completed = agent_msg(&chat.id, Some(MessageDelivery::pending(past)));
    msg_repo.create(&completed).await.unwrap();

    let due = msg_repo.find_due_deliveries(now, 50).await.unwrap();
    let due_ids: Vec<&str> = due.iter().map(|m| m.id.as_str()).collect();
    assert!(
        due_ids.contains(&completed.id.as_str()),
        "completed message must surface in retry queue"
    );
    assert!(
        !due_ids.contains(&executing.id.as_str()),
        "executing message must not surface in retry queue"
    );
}

#[tokio::test]
async fn resume_deliveries_skips_executing_messages() {
    let db = test_db().await;
    let chat_repo = SurrealChatRepo::new(db.clone());
    let msg_repo = SurrealMessageRepo::new(db.clone());

    let chat = channel_chat("channel:tg", "dm:1");
    chat_repo.create(&chat).await.unwrap();

    let now = Utc::now();
    let future = now + Duration::hours(1);

    let executing = executing_agent_msg(&chat.id, Some(MessageDelivery::pending(future)));
    let completed = agent_msg(&chat.id, Some(MessageDelivery::pending(future)));
    msg_repo.create(&executing).await.unwrap();
    msg_repo.create(&completed).await.unwrap();

    let updated = msg_repo
        .resume_deliveries_for_channel("channel:tg", now)
        .await
        .unwrap();
    assert_eq!(
        updated, 1,
        "only the Completed message should have been touched"
    );

    let due = msg_repo.find_due_deliveries(now, 50).await.unwrap();
    let due_ids: Vec<&str> = due.iter().map(|m| m.id.as_str()).collect();
    assert!(due_ids.contains(&completed.id.as_str()));
    assert!(!due_ids.contains(&executing.id.as_str()));
}
