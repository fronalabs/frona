use chrono::Utc;
use frona::agent::context::AgentContextKind;
use frona::agent::context::AgentContextSection;
use frona::agent::context::AgentContextSnapshot;
use frona::auth::User;
use frona::chat::message::models::Message;
use frona::chat::message::models::MessageRole;
use frona::core::repository::Repository;
use frona::db::init::setup_schema;
use frona::db::repo::agent_context_snapshots::SurrealAgentContextSnapshotRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::messages::SurrealMessageRepo;
use frona::db::repo::tool_calls::SurrealToolCallRepo;
use frona::db::repo::tool_calls::ToolCallRepository;
use frona::inference::hitl::Hitl;
use frona::inference::hitl::HitlRequest;
use frona::inference::tool_call::ToolCall;
use frona::inference::tool_call::ToolStatus;
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};

async fn database() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    setup_schema(&db).await.unwrap();
    db
}

#[tokio::test]
async fn legacy_native_user_and_message_rows_load_with_existing_defaults() {
    let db = database().await;
    db.query(
        "CREATE user:legacy SET handle = 'legacy-user', email = 'legacy@example.test',
        name = 'Legacy', password_hash = 'unchanged', timezone = 'UTC',
        deactivated_at = NONE, created_at = $now, updated_at = $now",
    )
    .bind(("now", Utc::now()))
    .await
    .unwrap()
    .check()
    .unwrap();
    let users = SurrealRepo::<User>::new(db.clone());
    let mut user = users.find_by_id("legacy").await.unwrap().unwrap();
    assert!(user.groups.is_empty());
    assert_eq!(user.handle.as_str(), "legacy-user");
    assert_eq!(user.password_hash, "unchanged");
    user.name = "Updated".into();
    users.update(&user).await.unwrap();
    assert_eq!(
        users.find_by_id("legacy").await.unwrap().unwrap().name,
        "Updated"
    );

    let messages = SurrealMessageRepo::new(db.clone());
    let mut message = Message::builder("chat", MessageRole::User, "original".into()).build();
    message.id = "legacy".into();
    messages.create(&message).await.unwrap();
    db.query("UPDATE message:legacy UNSET metadata, attachments, processed_content, agent_context_signature")
        .await.unwrap().check().unwrap();
    let stored = messages.find_by_id("legacy").await.unwrap().unwrap();
    assert_eq!(stored.role, MessageRole::User);
    assert_eq!(stored.inference_content(), "original");
    assert!(stored.metadata.is_empty());
    assert!(stored.attachments.is_empty());
    assert!(stored.agent_context_signature.is_none());
    db.query("UPDATE message:legacy SET metadata = 'invalid'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(messages.find_by_id("legacy").await.is_err());
}

#[tokio::test]
async fn tool_metadata_and_pending_human_input_survive_native_storage() {
    let db = database().await;
    let calls = SurrealToolCallRepo::new(db.clone());
    let call = ToolCall {
        id: "pending".into(),
        chat_id: "chat".into(),
        message_id: "response".into(),
        turn: 1,
        provider_call_id: "call-provider".into(),
        provider_item_id: Some("item-provider".into()),
        signature: Some("opaque-signature".into()),
        name: "ask".into(),
        arguments: serde_json::json!({"question": "Continue?"}),
        result: String::new(),
        success: true,
        duration_ms: 0,
        hitl: Some(Hitl {
            prompt: "Continue?".into(),
            url: String::new(),
            request: HitlRequest::Question {
                options: vec!["Yes".into(), "No".into()],
            },
            status: ToolStatus::Pending,
            response: None,
            delivery: None,
        }),
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        turn_reasoning: None,
        created_at: Utc::now(),
    };
    calls.create(&call).await.unwrap();
    let reopened = SurrealToolCallRepo::new(db);
    let stored = reopened
        .find_pending_by_chat_id("chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        serde_json::to_value(&call).unwrap()
    );
}

#[tokio::test]
async fn snapshots_retain_their_content_signature_and_reject_conflicting_content() {
    let db = database().await;
    let repo = SurrealAgentContextSnapshotRepo(db);
    let snapshot = AgentContextSnapshot::new(vec![AgentContextSection::prose(
        AgentContextKind::UserProfile,
        "profile",
        "User context".into(),
    )])
    .unwrap();
    repo.ensure_stored(&snapshot).await.unwrap();
    repo.ensure_stored(&snapshot).await.unwrap();
    assert_eq!(
        repo.find(&snapshot.signature).await.unwrap(),
        Some(snapshot.clone())
    );
    let mut conflicting = snapshot;
    conflicting.sections.clear();
    assert!(repo.ensure_stored(&conflicting).await.is_err());
}
