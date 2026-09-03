//! Verifies the architectural rule that `send_message` is only available in
//! the agent's heartbeat chat. Inside task chats and normal user chats the
//! tool is filtered out at `ChatSessionContext::build` so the model can't
//! satisfy a "send a reminder" instruction via `send_message` and then leave
//! `complete_task.result` empty against a non-nullable schema.

mod helpers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use frona::agent::models::Agent;
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::auth::User;
use frona::chat::models::CreateChatRequest;
use frona::chat::service::ChatService;
use frona::chat::session::{CancellationToken, ChatSessionContext};
use frona::core::config::Config;
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::init as db_init;
use frona::db::repo::agents::SurrealAgentRepo;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::conversation::DefaultConversationBuilder;
use frona::inference::registry::ModelProviderRegistry;
use frona::storage::StorageService;
use helpers::{MockModelProvider, test_model_group};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

fn workspace_resources() -> PathBuf {
    // Walk up from the test binary's CWD until we find a sibling `resources/prompts`.
    // Mirrors `tool/task_control.rs::tests::tool_with_schema`.
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

async fn build_state() -> (AppState, tempfile::TempDir) {
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

    // Replace the default chat_service with one wired to a mock provider so
    // model-group resolution succeeds. We don't run inference here - the tests
    // just inspect the tool registry the session builds - but `session.build`
    // resolves the agent's `model_group` against this registry.
    let provider: Arc<dyn frona::inference::provider::ModelProvider> =
        Arc::new(MockModelProvider::new(vec![]));
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
    // Rebuild Harness so it sees the new chat_service with the mock registry.
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

async fn seed_user_and_agent(state: &AppState) {
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
    let _ = repo
        .create(&Agent {
            id: "agent-1".into(),
            user_id: "user-1".into(),
            handle: frona::handle!("agent-1"),
            name: "Test Agent".into(),
            description: String::new(),
            model_group: "primary".into(),
            enabled: true,
            skills: None,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            avatar: None,
            identity: Default::default(),
            prompt: Some("You are a test agent.".into()),
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        })
        .await;
}

async fn build_session_for_chat(state: &AppState, chat_id: &str) -> ChatSessionContext {
    let chat = state
        .chat_service
        .find_chat(chat_id)
        .await
        .unwrap()
        .expect("chat exists");
    let builder = Box::new(DefaultConversationBuilder {
        user_service: state.user_service.clone(),
        storage_service: state.storage_service.clone(),
        agent_service: state.agent_service.clone(),
    });
    ChatSessionContext::build(
        &state.harness,
        "user-1",
        chat,
        CancellationToken::new(),
        builder,
    )
    .await
    .expect("session builds")
}

fn registry_has_tool(session: &ChatSessionContext, tool_id: &str) -> bool {
    session
        .tool_registry
        .definitions()
        .iter()
        .any(|d| d.id == tool_id)
}

/// Heartbeat path: the agent's `heartbeat_chat_id` points at the current chat,
/// so `send_message` IS registered - this is the one context where the agent
/// has no other channel to reach the user.
#[tokio::test]
async fn send_message_registered_in_heartbeat_chat() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Heartbeat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    state
        .agent_service
        .update_heartbeat_chat("agent-1", &chat.id)
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &chat.id).await;
    assert!(
        registry_has_tool(&session, "send_message"),
        "send_message must be available in the heartbeat chat (the only context where autonomous outreach has no other channel)"
    );
}

/// Task-execution path: the chat has `task_id` set, so `send_message` is
/// filtered out. The agent must deliver via `complete_task.result`. This is
/// the regression guard for the trace bug.
#[tokio::test]
async fn send_message_filtered_in_task_chat() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    // Persist a Task so the chat row's `task_id` is referentially honest.
    let task_repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    let now = Utc::now();
    let task = Task {
        id: frona::core::repository::new_id(),
        user_id: "user-1".into(),
        agent_id: "agent-1".into(),
        space_id: None,
        chat_id: None,
        title: "Send reminder".into(),
        description: "Send a friendly reminder to drink water.".into(),
        status: TaskStatus::InProgress,
        kind: TaskKind::Direct {
            source_chat_id: None,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        quarantined: false,
        result_schema: Some(serde_json::json!({"type": "string"})),
        result_description: None,
        created_at: now,
        updated_at: now,
    };
    task_repo.create(&task).await.unwrap();

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: Some(task.id.clone()),
                agent_id: "agent-1".to_string(),
                title: Some("Task chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &chat.id).await;
    assert!(
        !registry_has_tool(&session, "send_message"),
        "send_message must be hidden inside a task chat — `complete_task.result` is the delivery channel"
    );
}

/// Normal user chat (no task, no heartbeat): `send_message` is filtered out
/// under the Strict rule. The agent already replies by streaming text into
/// the current chat; `send_message` would be redundant.
#[tokio::test]
async fn send_message_filtered_in_normal_chat() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Plain chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &chat.id).await;
    assert!(
        !registry_has_tool(&session, "send_message"),
        "send_message must be hidden in a normal user chat under the Strict rule"
    );
}

/// Sanity check the heartbeat allowance is keyed on identity, not presence -
/// a second chat owned by the same agent (not its `heartbeat_chat_id`) should
/// still be filtered. Prevents accidentally widening the rule to "agent has
/// heartbeat configured anywhere".
#[tokio::test]
async fn send_message_filtered_in_non_heartbeat_chat_of_heartbeat_agent() {
    let (state, _tmp) = build_state().await;
    seed_user_and_agent(&state).await;

    let hb_chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Heartbeat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();
    state
        .agent_service
        .update_heartbeat_chat("agent-1", &hb_chat.id)
        .await
        .unwrap();

    let other_chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Other".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let session = build_session_for_chat(&state, &other_chat.id).await;
    assert!(
        !registry_has_tool(&session, "send_message"),
        "send_message must remain hidden in non-heartbeat chats even when the agent has a heartbeat elsewhere"
    );
}
