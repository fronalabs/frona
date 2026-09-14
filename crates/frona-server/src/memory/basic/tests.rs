use super::*;
use crate::core::config::Config;
use crate::db::repo::generic::SurrealRepo;
use crate::inference::config::ModelRegistryConfig;
use surrealdb::{Surreal, engine::local::Mem};

#[tokio::test]
async fn scheduled_memory_uses_latest_chat_in_scope_or_explicit_memory_group() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let fixture = crate::app_state_fixture::build(&db).await;
    let config: Config = serde_json::from_value(serde_json::json!({
        "providers": {"mock": {"provider": "openai"}},
        "models": {
            "older": {"provider": "mock", "model": "older", "context_window": 4096},
            "newer": {"provider": "mock", "model": "newer", "context_window": 4096},
            "unrelated": {"provider": "mock", "model": "unrelated", "context_window": 4096},
            "dedicated": {"provider": "mock", "model": "memory", "context_window": 4096}
        }
    }))
    .unwrap();
    let groups = ModelRegistryConfig {
        providers: config.providers,
        models: config.models,
        skip_auto_discover: true,
    }
    .parse_model_groups(&config.inference, Default::default())
    .unwrap();
    let service = fixture.state.model_provider_service.clone();
    let providers = ModelProviderService::new(
        service.directory,
        service.config_service,
        service.store,
        service.validator,
        service.runtime,
        service.active,
        groups,
        Default::default(),
    );
    let agents = &fixture.state.agent_service;
    let now = Utc::now();
    let users: SurrealRepo<crate::auth::User> = SurrealRepo::new(db.clone());
    for id in ["user", "other-user"] {
        let user = serde_json::from_value(serde_json::json!({
            "id": id, "handle": id, "email": format!("{id}@example.com"),
            "name": id, "password_hash": "", "groups": [],
            "created_at": now, "updated_at": now,
        }))
        .unwrap();
        users.create(&user).await.unwrap();
    }
    for (index, group) in ["older", "newer", "unrelated"].into_iter().enumerate() {
        let user_id = if group == "unrelated" {
            "other-user"
        } else {
            "user"
        };
        let agent = agents
            .create(
                user_id,
                serde_json::from_value(serde_json::json!({
                    "name": group, "description": "", "model_group": group,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let chat: crate::chat::models::Chat = serde_json::from_value(serde_json::json!({
            "id": format!("chat-{group}"), "user_id": user_id,
            "agent_id": agent.id, "space_id": if group == "unrelated" { "other-space" } else { "space" },
            "created_at": now, "updated_at": now + chrono::Duration::seconds(index as i64),
        })).unwrap();
        let repo: SurrealChatRepo = SurrealRepo::new(db.clone());
        repo.create(&chat).await.unwrap();
    }
    let mut memory = BasicMemoryService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        Arc::new(providers),
        PromptLoader::new(std::path::PathBuf::from("resources/prompts")),
        fixture.state.usage_service.clone(),
        MemoryConfig::default(),
    );
    let user_chats = memory.chat_repo.find_by_user_id("user").await.unwrap();
    let space_chats = memory.chat_repo.find_by_space_id("space").await.unwrap();
    for chats in [&user_chats, &space_chats] {
        assert_eq!(
            memory
                .compaction_model_group_for_chats(agents, chats)
                .await
                .unwrap()
                .name,
            "newer"
        );
    }
    memory.memory_config.model_group.clear();
    assert_eq!(
        memory
            .compaction_model_group_for_chats(agents, &user_chats)
            .await
            .unwrap()
            .name,
        "newer"
    );
    assert!(
        memory
            .compaction_model_group_for_chats(agents, &[])
            .await
            .is_err()
    );

    memory.memory_config.model_group = "dedicated".into();
    for chats in [user_chats.as_slice(), space_chats.as_slice(), &[]] {
        assert_eq!(
            memory
                .compaction_model_group_for_chats(agents, chats)
                .await
                .unwrap()
                .name,
            "dedicated"
        );
    }
}
