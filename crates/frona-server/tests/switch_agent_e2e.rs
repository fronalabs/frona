//! `/<agent-handle>` and `@<agent-handle>` invocations re-attribute the
//! turn's reply to the target agent - verified end-to-end through the
//! harness with a mock LLM.

mod helpers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use frona::agent::models::Agent;
use frona::auth::User;
use frona::chat::message::models::MessageCommand;
use frona::chat::models::CreateChatRequest;
use frona::chat::service::ChatService;
use frona::chat::session::CancellationToken;
use frona::core::config::Config;
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::init as db_init;
use frona::db::repo::agents::SurrealAgentRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::conversation::DefaultConversationBuilder;
use frona::inference::registry::ModelProviderRegistry;
use frona::storage::StorageService;
use helpers::{MockModelProvider, MockResponse, test_model_group};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

fn workspace_resources() -> PathBuf {
    std::env::current_dir()
        .expect("cwd")
        .ancestors()
        .find(|p| p.join("resources/prompts").exists())
        .expect("workspace resources/ not found from cwd")
        .join("resources")
}

fn test_config(tmp: &tempfile::TempDir) -> Config {
    let base = tmp.path().to_string_lossy().to_string();
    let resources = workspace_resources();
    Config {
        server: frona::core::config::ServerConfig {
            port: 0,
            static_dir: format!("{base}/static"),
            max_concurrent_tasks: 10,
            ..Default::default()
        },
        auth: frona::core::config::AuthConfig {
            encryption_secret: "test-secret".to_string(),
            ..Default::default()
        },
        database: frona::core::config::DatabaseConfig {
            path: format!("{base}/db"),
        },
        storage: frona::core::config::StorageConfig {
            data_dir: format!("{base}/data"),
            shared_config_dir: resources.to_string_lossy().into_owned(),
            skills_dir: format!("{base}/skills"),
            cache_dir: format!("{base}/cache"),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn build_state(
    provider: Arc<dyn frona::inference::provider::ModelProvider>,
) -> (AppState, tempfile::TempDir) {
    let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
    db_init::setup_schema(&db).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let storage = StorageService::new(&config);
    let resource_manager = Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let metrics_handle = frona::core::metrics::setup_metrics_recorder();

    let mut state = AppState::new(
        db.clone(),
        &config,
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage,
        metrics_handle,
        resource_manager,
    );

    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    let mut groups = HashMap::new();
    groups.insert("primary".to_string(), test_model_group());
    let mock_registry = ModelProviderRegistry::for_testing(providers, groups);

    let chat_service = ChatService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        state.agent_service.clone(),
        mock_registry,
        state.storage_service.clone(),
        state.user_service.clone(),
        state.prompts.clone(),
        state.broadcast_service.clone(),
        state.presign_service.clone(),
        state.usage_service.clone(),
    );
    state.chat_service = chat_service.clone();
    state.harness = Arc::new(frona::agent::harness::Harness::new(
        chat_service,
        state.user_service.clone(),
        state.storage_service.clone(),
        state.agent_service.clone(),
        helpers::test_memory_service(&state, &db),
        state.skill_service.clone(),
        state.task_service.clone(),
        state.vault_service.clone(),
        state.mcp_service.clone(),
        state.tool_manager.clone(),
        state.policy_service.clone(),
        state.broadcast_service.clone(),
        state.active_sessions.clone(),
        state.execution_registry.clone(),
        state.shutdown_token.clone(),
        state.prompts.clone(),
        state.config.clone(),
        state.usage_service.clone(),
    ));
    state.task_executor = Arc::new(frona::agent::task::executor::TaskExecutor::new(
        state.harness.clone(),
    ));

    state.tool_manager.init(&state);
    state.policy_service.sync_base_policies().await.unwrap();
    (state, tmp)
}

async fn seed_user_and_two_agents(state: &AppState) -> (String, String) {
    let now = Utc::now();
    let _ = state
        .user_service
        .create(&User {
            id: "user-1".into(),
            handle: frona::handle!("user-1"),
            email: "user-1@test.com".into(),
            name: "Test User".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        })
        .await;

    let repo = SurrealAgentRepo::new(state.db.clone());
    let default_agent = Agent {
        id: "agent-default".into(),
        user_id: "user-1".into(),
        handle: frona::handle!("default"),
        name: "Default".into(),
        description: String::new(),
        model_group: "primary".into(),
        enabled: true,
        skills: None,
        sandbox_limits: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: Default::default(),
        prompt: Some("You are the default agent.".into()),
        heartbeat_interval: None,
        next_heartbeat_at: None,
        heartbeat_chat_id: None,
        created_at: now,
        updated_at: now,
    };
    let target_agent = Agent {
        id: "agent-target".into(),
        handle: frona::handle!("target"),
        name: "Target".into(),
        prompt: Some("You are the target agent.".into()),
        ..default_agent.clone()
    };
    let _ = repo.create(&default_agent).await;
    let _ = repo.create(&target_agent).await;
    (default_agent.id, target_agent.id)
}

#[tokio::test]
async fn switch_agent_command_reattributes_response() {
    let provider: Arc<dyn frona::inference::provider::ModelProvider> = Arc::new(
        MockModelProvider::new(vec![MockResponse::Text("ack from target".into())]),
    );
    let (state, _tmp) = build_state(provider).await;
    let (default_agent_id, target_agent_id) = seed_user_and_two_agents(&state).await;

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: default_agent_id.clone(),
                title: Some("test chat".into()),
                metadata: None,
            },
        )
        .await
        .expect("chat creates");

    let user_msg = state
        .chat_service
        .create_stream_user_message(
            "user-1",
            &chat.id,
            "@target hi",
            Vec::new(),
            Some(MessageCommand::Command {
                name: "target".into(),
                args: "hi".into(),
            }),
        )
        .await
        .expect("user message persists");
    assert!(
        matches!(user_msg.command, Some(MessageCommand::Command { ref name, .. }) if name == "target"),
        "precondition: user message carries the Command invocation",
    );

    let agent_msg = state
        .chat_service
        .create_executing_agent_message(&chat.id, &default_agent_id)
        .await
        .expect("placeholder persists");
    assert_eq!(
        agent_msg.agent_id.as_deref(),
        Some(default_agent_id.as_str()),
        "precondition: placeholder starts attributed to default agent",
    );

    // `run_turn` calls `finalize` internally; `run_loop` doesn't, and
    // without finalize the DB row keeps the placeholder's `agent_id`.
    let builder = Box::new(DefaultConversationBuilder {
        user_service: state.user_service.clone(),
        storage_service: state.storage_service.clone(),
        agent_service: state.agent_service.clone(),
    });
    state
        .harness
        .run_turn(
            "user-1",
            &chat.id,
            &agent_msg.id,
            CancellationToken::new(),
            builder,
            &[],
            None,
        )
        .await;

    let stored = state
        .chat_service
        .get_message("user-1", &agent_msg.id)
        .await
        .expect("response row exists");
    assert_eq!(
        stored.agent_id.as_deref(),
        Some(target_agent_id.as_str()),
        "persisted response should be attributed to target agent",
    );
    assert_eq!(stored.content, "ack from target");

    let chat_after = state
        .chat_service
        .find_chat(&chat.id)
        .await
        .unwrap()
        .expect("chat exists");
    assert_eq!(
        chat_after.agent_id, default_agent_id,
        "chat's persistent agent should NOT change",
    );
}
