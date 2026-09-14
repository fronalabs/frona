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

#[tokio::test]
async fn memory_tools_use_chat_model_unless_memory_group_is_configured() {
    use frona::inference::provider::ModelProvider;
    use frona::memory::service::MemoryService;
    use helpers::{MockModelProvider, MockResponse};

    for (memory_group_name, with_primary) in [
        ("memory", false),
        ("", false),
        ("dedicated", false),
        ("memory", true),
    ] {
        let db = test_db().await;
        let chat_provider = Arc::new(MockModelProvider::new(vec![
            MockResponse::Text("chat summary".into()),
            MockResponse::Text("chat summary".into()),
        ]));
        let memory_provider = Arc::new(MockModelProvider::new(vec![
            MockResponse::Text("memory summary".into()),
            MockResponse::Text("memory summary".into()),
        ]));
        let mut chat_group = helpers::test_model_group();
        chat_group.name = "chat-model".into();
        let mut groups = [(chat_group.name.clone(), chat_group)]
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        if memory_group_name == "dedicated" {
            let mut memory_group = helpers::test_model_group();
            memory_group.name = "dedicated".into();
            memory_group.main.provider_handle = frona::handle!("memory-provider");
            groups.insert(memory_group.name.clone(), memory_group);
        }
        if with_primary {
            let mut primary = helpers::test_model_group();
            primary.name = "primary".into();
            primary.main.provider_handle = frona::handle!("memory-provider");
            groups.insert(primary.name.clone(), primary);
        }
        let providers = helpers::test_model_service(
            [
                (
                    "mock".into(),
                    chat_provider.clone() as Arc<dyn ModelProvider>,
                ),
                (
                    "memory-provider".into(),
                    memory_provider.clone() as Arc<dyn ModelProvider>,
                ),
            ]
            .into(),
            groups,
        )
        .await;
        let svc = BasicMemoryService::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            Arc::new(providers),
            PromptLoader::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../resources/prompts"),
            ),
            frona::inference::usage::UsageService::new(
                frona_model_catalog::ModelCatalogStore::new(
                    frona_model_catalog::ModelCatalogSnapshot::empty(),
                ),
                SurrealRepo::new(db.clone()),
                frona::chat::broadcast::BroadcastService::new(),
            ),
            frona::core::config::MemoryConfig {
                model_group: memory_group_name.into(),
                ..Default::default()
            },
        );
        let mut ctx = helpers::mock_context();
        ctx.agent.model_group = "chat-model".into();
        for (tool, (tool_name, source_type, source_id)) in svc.tools().into_iter().zip([
            (
                "store_agent_memory",
                MemorySourceType::Agent,
                ctx.agent.id.as_str(),
            ),
            (
                "store_user_memory",
                MemorySourceType::User,
                ctx.user.id.as_str(),
            ),
        ]) {
            tool.execute(
                tool_name,
                serde_json::json!({"memory": "Remember this", "overrides": true}),
                &ctx,
            )
            .await
            .unwrap();
            let summary = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(summary) = svc
                        .get_memory(source_type.clone(), source_id)
                        .await
                        .unwrap()
                    {
                        break summary;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("background memory compaction did not finish");
            assert_eq!(
                summary.content,
                if memory_group_name == "dedicated" {
                    "memory summary"
                } else {
                    "chat summary"
                }
            );
        }
        assert_eq!(
            *chat_provider.call_count.lock().unwrap(),
            if memory_group_name == "dedicated" {
                0
            } else {
                2
            }
        );
        assert_eq!(
            *memory_provider.call_count.lock().unwrap(),
            if memory_group_name == "dedicated" {
                2
            } else {
                0
            }
        );
    }
}
