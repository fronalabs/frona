//! Server-owned schema installation accepts both pre-extraction and adopted data.
mod helpers;

use frona::core::repository::Repository;
use frona::db::{
    init::{initialize_schema, setup_schema},
    repo::generic::SurrealRepo,
};
use serde_json::Value;
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};

async fn database() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db.use_ns("frona").use_db("frona").await.unwrap();
    db
}

async fn marker(db: &Surreal<Db>) -> Option<u64> {
    db.query("RETURN $frona_harness_schema_version")
        .await
        .unwrap()
        .take(0)
        .unwrap()
}

async fn info(db: &Surreal<Db>) -> Value {
    db.query("INFO FOR DB")
        .await
        .unwrap()
        .take::<surrealdb::types::Value>(0)
        .unwrap()
        .into_json_value()
}

#[tokio::test]
async fn base_and_extracted_schemas_preserve_native_rows_and_marker_until_migration() {
    for (schema, adopted) in [
        (include_str!("fixtures/schema/base-server.surql"), false),
        (include_str!("fixtures/schema/extracted-v1.surql"), true),
    ] {
        let db = database().await;
        db.query(schema).await.unwrap().check().unwrap();
        if adopted {
            db.query("DEFINE PARAM $frona_harness_schema_version VALUE 1")
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let user = helpers::mock_context().user;
        let users = SurrealRepo::<frona::auth::User>::new(db.clone());
        users.create(&user).await.unwrap();
        let message = frona::chat::message::models::Message::builder(
            "saved-chat",
            frona::chat::message::models::MessageRole::User,
            "saved content".into(),
        )
        .build();
        let messages = SurrealRepo::new(db.clone());
        messages.create(&message).await.unwrap();
        for _ in 0..2 {
            setup_schema(&db).await.unwrap();
            assert_eq!(marker(&db).await, adopted.then_some(1));
            assert_eq!(
                serde_json::to_value(users.find_by_id(&user.id).await.unwrap()).unwrap(),
                serde_json::to_value(Some(user.clone())).unwrap()
            );
            assert_eq!(
                messages.find_by_id(&message.id).await.unwrap(),
                Some(message.clone())
            );
        }
        let tables = info(&db).await;
        for name in [
            "user",
            "agent",
            "chat",
            "message",
            "task",
            "tool_call",
            "policy",
            "user_group",
            "user_config",
            "chat_summary",
            "agent_context_snapshot",
            "inference_usage",
            "vault_connection",
            "channel",
        ] {
            assert!(
                tables["tables"].get(name).is_some(),
                "missing server table {name}"
            );
        }
        initialize_schema(&db).await.unwrap();
        initialize_schema(&db).await.unwrap();
        assert_eq!(marker(&db).await, Some(1));
        assert_eq!(
            serde_json::to_value(users.find_by_id(&user.id).await.unwrap()).unwrap(),
            serde_json::to_value(Some(user)).unwrap()
        );
        assert_eq!(
            messages.find_by_id(&message.id).await.unwrap(),
            Some(message)
        );
    }
}

#[tokio::test]
async fn unsupported_schema_markers_are_rejected_before_any_schema_or_migration_mutation() {
    for unsupported in ["99", "'invalid'"] {
        let db = database().await;
        db.query("CREATE sentinel:kept CONTENT {value: 'unchanged'}")
            .await
            .unwrap()
            .check()
            .unwrap();
        db.query(format!(
            "DEFINE PARAM $frona_harness_schema_version VALUE {unsupported}"
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
        let before = info(&db).await;
        assert!(setup_schema(&db).await.is_err());
        assert_eq!(info(&db).await, before);
        assert!(initialize_schema(&db).await.is_err());
        assert_eq!(info(&db).await, before);
        let value: Option<String> = db
            .query("RETURN (SELECT * FROM sentinel:kept)[0].value")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(value.as_deref(), Some("unchanged"));
    }
}

#[tokio::test]
async fn failed_migration_does_not_adopt_the_extracted_schema_marker() {
    let db = database().await;
    setup_schema(&db).await.unwrap();
    db.query("DEFINE EVENT refuse_cursor ON TABLE runtime_config WHEN $event = 'CREATE' THEN { THROW 'injected migration cursor failure'; };").await.unwrap().check().unwrap();
    assert!(initialize_schema(&db).await.is_err());
    assert_eq!(marker(&db).await, None);
    db.query("REMOVE EVENT refuse_cursor ON TABLE runtime_config")
        .await
        .unwrap()
        .check()
        .unwrap();
    initialize_schema(&db).await.unwrap();
    assert_eq!(marker(&db).await, Some(1));
    let cursors: Vec<String> = db
        .query("SELECT VALUE value FROM runtime_config WHERE key = 'db_schema_version'")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(cursors.len(), 1);
}
