use std::sync::Arc;

use chrono::Utc;
use frona::agent::prompt::PromptLoader;
use frona::chat::message::models::{Message, MessageDelivery, MessageRole};
use frona::chat::message::repository::MessageRepository;
use frona::core::repository::Repository;
use frona::db::repo::generic::SurrealRepo;
use frona::db::repo::messages::SurrealMessageRepo;
use frona::inference::conversation::{
    ConversationBuilder, ConversationContext, DefaultConversationBuilder,
};
use frona::memory::basic::BasicMemoryService;
use frona::memory::basic::models::MemorySourceType;
use frona::memory::basic::repository::MemoryRepository;
use frona::memory::service::MemoryService;
use frona::storage::Attachment;
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};

mod helpers;

async fn database() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    db
}

fn populated_message() -> Message {
    let mut delivery = MessageDelivery::pending(Utc::now());
    delivery.attempts = 3;
    delivery.tool_index = 2;
    Message::builder(
        "legacy-chat",
        MessageRole::User,
        "Remember my preference".into(),
    )
    .attachments(vec![Attachment {
        filename: "notes.txt".into(),
        content_type: "text/plain".into(),
        size_bytes: 12,
        owner: "user".into(),
        path: "notes.txt".into(),
        url: None,
    }])
    .metadata(
        [(
            "source".into(),
            serde_json::json!({"channel": "web", "revision": 2}),
        )]
        .into(),
    )
    .delivery(delivery)
    .build()
}

async fn assert_absent_field_defaults(field: &str) {
    let db = database().await;
    let repo: SurrealMessageRepo = SurrealRepo::new(db.clone());
    let original = populated_message();
    repo.create(&original).await.unwrap();
    db.query(format!("UPDATE type::record('message', $id) UNSET {field}"))
        .bind(("id", original.id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let mut expected = original.clone();
    match field {
        "attachments" => expected.attachments.clear(),
        "metadata" => expected.metadata.clear(),
        "delivery.attempts" => expected.delivery.as_mut().unwrap().attempts = 0,
        "delivery.tool_index" => expected.delivery.as_mut().unwrap().tool_index = 0,
        _ => unreachable!(),
    }
    assert_eq!(
        repo.find_by_id(&original.id).await.unwrap().unwrap(),
        expected,
        "{field}"
    );
    assert_eq!(
        repo.find_by_chat_id(&original.chat_id).await.unwrap(),
        vec![expected]
    );
    repo.delete(&original.id).await.unwrap();
}

#[tokio::test]
async fn absent_attachments_default_without_overwriting_other_values() {
    assert_absent_field_defaults("attachments").await;
}

#[tokio::test]
async fn absent_metadata_defaults_without_overwriting_other_values() {
    assert_absent_field_defaults("metadata").await;
}

#[tokio::test]
async fn absent_delivery_attempts_default_without_overwriting_other_values() {
    assert_absent_field_defaults("delivery.attempts").await;
}

#[tokio::test]
async fn absent_delivery_tool_index_defaults_without_overwriting_other_values() {
    assert_absent_field_defaults("delivery.tool_index").await;
}

#[tokio::test]
async fn explicit_message_values_round_trip_unchanged() {
    let db = database().await;
    let repo: SurrealMessageRepo = SurrealRepo::new(db);
    let original = populated_message();
    repo.create(&original).await.unwrap();
    let loaded = repo.find_by_id(&original.id).await.unwrap().unwrap();
    assert_eq!(loaded, original);
    repo.update(&loaded).await.unwrap();
    assert_eq!(
        repo.find_by_chat_id(&original.chat_id).await.unwrap(),
        vec![original]
    );
}

#[tokio::test]
async fn null_and_malformed_message_fields_still_fail_database_reads() {
    let db = database().await;
    let repo: SurrealMessageRepo = SurrealRepo::new(db.clone());
    for (field, invalid) in [
        ("attachments", "'not-an-array'"),
        ("attachments", "[{}]"),
        ("metadata", "[]"),
        ("delivery.attempts", "'three'"),
        ("delivery.attempts", "-1"),
        ("delivery.tool_index", "'two'"),
        ("delivery.tool_index", "-1"),
    ] {
        for value in ["NULL", invalid] {
            let original = populated_message();
            repo.create(&original).await.unwrap();
            db.query(format!(
                "UPDATE type::record('message', $id) SET {field} = {value}"
            ))
            .bind(("id", original.id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
            let error = repo.find_by_id(&original.id).await.unwrap_err().to_string();
            assert!(
                error.contains(field.rsplit('.').next().unwrap()),
                "{field} = {value}: {error}"
            );
            assert!(repo.find_by_chat_id(&original.chat_id).await.is_err());
            repo.delete(&original.id).await.unwrap();
        }
    }
}

#[tokio::test]
async fn optional_delivery_remains_optional_but_malformed_delivery_fails() {
    let db = database().await;
    let repo: SurrealMessageRepo = SurrealRepo::new(db.clone());
    for value in ["NONE", "NULL", "'invalid'"] {
        let original = populated_message();
        repo.create(&original).await.unwrap();
        db.query(format!(
            "UPDATE type::record('message', $id) SET delivery = {value}"
        ))
        .bind(("id", original.id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
        if value != "NONE" {
            assert!(repo.find_by_id(&original.id).await.is_err());
        } else {
            assert!(
                repo.find_by_id(&original.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .delivery
                    .is_none()
            );
        }
        repo.delete(&original.id).await.unwrap();
    }
}

#[tokio::test]
async fn old_message_rows_build_conversation_history() {
    let db = database().await;
    let fixture = helpers::app_state::build(&db).await;
    let repo: SurrealMessageRepo = SurrealRepo::new(db.clone());
    let original = populated_message();
    repo.create(&original).await.unwrap();
    db.query("UPDATE message UNSET attachments, metadata, delivery.attempts, delivery.tool_index")
        .await
        .unwrap()
        .check()
        .unwrap();
    let loaded = fixture
        .state
        .chat_service
        .compactor()
        .load_conversation(&original.chat_id)
        .await
        .unwrap();
    assert_eq!(loaded.messages.len(), 1);
    let builder = DefaultConversationBuilder {
        user_service: fixture.state.user_service.clone(),
        storage_service: fixture.state.storage_service.clone(),
        agent_service: fixture.state.agent_service.clone(),
    };
    let history = builder
        .build(
            &loaded.messages,
            &[],
            &ConversationContext {
                agent_id: "agent".into(),
                user_id: "user".into(),
                model_config: helpers::test_model_group().main,
            },
            loaded.summary.as_deref(),
        )
        .await;
    assert_eq!(history.row_ids, vec![original.id]);
    assert_eq!(
        history.history,
        vec![rig_core::completion::Message::user(original.content)]
    );
}

#[tokio::test]
async fn memory_maintenance_consumes_a_chat_with_legacy_message_rows() {
    let db = database().await;
    let mut fixture = helpers::app_state::build(&db).await;
    // Register the real maintenance job; no private sweep method is exposed for tests.
    Arc::make_mut(&mut fixture.state.config)
        .memory
        .basic_space_compaction_secs = 1;
    let provider = Arc::new(helpers::MockModelProvider::new(vec![
        helpers::MockResponse::Text("Retained space memory".into()),
    ]));
    let group = helpers::test_model_group();
    let providers = helpers::test_model_service(
        [(
            "mock".into(),
            provider.clone() as Arc<dyn frona::inference::provider::ModelProvider>,
        )]
        .into(),
        [(group.name.clone(), group)].into(),
    )
    .await;
    let memory = BasicMemoryService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        Arc::new(providers),
        PromptLoader::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
        ),
        fixture.state.usage_service.clone(),
        frona::core::config::MemoryConfig {
            model_group: "test".into(),
            ..Default::default()
        },
    );
    let now = Utc::now();
    let spaces: SurrealRepo<frona::space::models::Space> = SurrealRepo::new(db.clone());
    spaces
        .create(&frona::space::models::Space {
            id: "space".into(),
            user_id: "user".into(),
            name: "Older chats".into(),
            metadata: Default::default(),
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let chats: SurrealRepo<frona::chat::models::Chat> = SurrealRepo::new(db.clone());
    chats
        .create(
            &serde_json::from_value(serde_json::json!({
                "id": "legacy-chat", "user_id": "user", "agent_id": "agent", "space_id": "space",
                "title": "Older conversation", "created_at": now, "updated_at": now,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let summaries: SurrealRepo<frona::chat::models::ChatSummary> = SurrealRepo::new(db.clone());
    summaries
        .create(&frona::chat::models::ChatSummary {
            id: "summary".into(),
            chat_id: "legacy-chat".into(),
            user_id: "user".into(),
            content: "The user prefers Rust".into(),
            item_count: 2,
            compacted_until: now - chrono::Duration::seconds(1),
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let messages: SurrealMessageRepo = SurrealRepo::new(db.clone());
    messages.create(&populated_message()).await.unwrap();
    db.query("UPDATE message UNSET attachments, metadata, delivery.attempts, delivery.tool_index")
        .await
        .unwrap()
        .check()
        .unwrap();

    let shutdown = fixture.state.shutdown_token.clone();
    memory.register_maintenance(&frona::scheduler::Scheduler::new(fixture.state.clone()));
    let memories: SurrealRepo<frona::memory::basic::models::Memory> = SurrealRepo::new(db);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(stored) = memories
                .find_latest(MemorySourceType::Space, "space")
                .await
                .unwrap()
            {
                break stored;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    shutdown.cancel();
    let stored = result.expect("maintenance must load legacy messages and persist space memory");
    assert_eq!(stored.content, "Retained space memory");
    assert_eq!(stored.metadata["chat_count"], 1);
    assert_eq!(provider.calls(), 1);
    let request = serde_json::to_string(&provider.last_history()).unwrap();
    assert!(request.contains("Older conversation"));
    assert!(request.contains("The user prefers Rust"));
}
