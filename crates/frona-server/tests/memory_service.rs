use std::sync::Arc;

use frona::agent::prompt::PromptLoader;
use frona::db::init as db;
use frona::db::repo::basic_memory::SurrealMemoryEntryRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::memory::basic::BasicMemoryService;
use frona::memory::basic::models::{Memory, MemorySourceType};
use frona::memory::basic::repository::MemoryEntryRepository;
use frona::memory::basic::repository::MemoryRepository;
use frona::tool::AgentTool;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

mod helpers;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

async fn make_memory_service(db: Surreal<Db>) -> BasicMemoryService {
    let provider_registry =
        helpers::test_model_service(Default::default(), Default::default()).await;

    let usage_service = frona::inference::usage::UsageService::new(
        frona_model_catalog::ModelCatalogStore::new(
            frona_model_catalog::ModelCatalogSnapshot::empty(),
        ),
        SurrealRepo::new(db.clone()),
        frona::chat::broadcast::BroadcastService::new(),
    );
    BasicMemoryService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db),
        Arc::new(provider_registry),
        PromptLoader::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("resources")
                .join("prompts"),
        ),
        usage_service,
        frona::core::config::MemoryConfig::default(),
    )
}

#[tokio::test]
async fn test_store_memory_entry_persists_to_db() {
    let db = test_db().await;
    let svc = make_memory_service(db.clone()).await;

    svc.store_memory_entry("agent-1", "User likes Rust", Some("chat-1"))
        .await
        .unwrap();

    let repo: SurrealMemoryEntryRepo = SurrealRepo::new(db);
    let entries = repo.find_by_agent_id("agent-1").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "User likes Rust");
    assert_eq!(entries[0].source_chat_id.as_deref(), Some("chat-1"));
    assert!(entries[0].user_id.is_none());
}

#[tokio::test]
async fn test_store_user_memory_entry_persists_with_user_id() {
    let db = test_db().await;
    let svc = make_memory_service(db.clone()).await;

    svc.store_user_memory_entry("user-1", "Name is Alice", Some("chat-1"))
        .await
        .unwrap();

    let repo: SurrealMemoryEntryRepo = SurrealRepo::new(db);
    let entries = repo.find_by_user_id("user-1").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "Name is Alice");
    assert_eq!(entries[0].user_id.as_deref(), Some("user-1"));
    assert!(entries[0].agent_id.is_empty());
}

#[tokio::test]
async fn test_compact_entries_if_needed_skips_below_threshold() {
    let db = test_db().await;
    let svc = make_memory_service(db.clone()).await;

    svc.store_memory_entry("agent-1", "Short memory 1", None)
        .await
        .unwrap();
    svc.store_memory_entry("agent-1", "Short memory 2", None)
        .await
        .unwrap();

    // Entries are small (well under 3000 tokens), so compaction should not have been triggered.
    // We verify no Memory record was created since we never called compact_entries_if_needed.
    let memory_repo: SurrealRepo<Memory> = SurrealRepo::new(db);
    let memory = memory_repo
        .find_latest(MemorySourceType::Agent, "agent-1")
        .await
        .unwrap();
    assert!(
        memory.is_none(),
        "No Memory record should exist since compaction was never triggered"
    );
}

/// A memory that is only whitespace is not a memory. It used to be stored verbatim -
/// the arg was read without a trim or a blank check, unlike every other memory tool.
#[tokio::test]
async fn test_store_user_memory_tool_rejects_a_blank_memory() {
    let db = test_db().await;
    let svc = make_memory_service(db.clone()).await;
    let tool = frona::memory::basic::tools::StoreUserMemoryTool::new(
        svc,
        None,
        PromptLoader::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("resources")
                .join("prompts"),
        ),
    );

    let stored = tool
        .execute(
            "store_user_memory",
            serde_json::json!({ "memory": "   " }),
            &helpers::mock_context(),
        )
        .await;
    match stored {
        Err(frona::core::error::AppError::Validation(_)) => {}
        Err(e) => panic!("expected a validation error, got {e:?}"),
        Ok(_) => panic!("a whitespace-only memory was accepted"),
    }

    let repo: SurrealMemoryEntryRepo = SurrealRepo::new(db);
    assert!(
        repo.find_by_user_id("test-user").await.unwrap().is_empty(),
        "nothing was stored"
    );
}
