mod helpers;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::Utc;
use frona::agent::models::Agent;
use frona::auth::User;
use frona::chat::message::models::{Message, MessageRole};
use frona::chat::models::{Chat, CreateChatRequest};
use frona::chat::service::ChatService;
use frona::core::config::Config;
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::init as db_init;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::registry::ModelProviderRegistry;
use frona::space::models::Space;
use frona::storage::StorageService;
use helpers::{MockModelProvider, MockResponse, init_metrics, test_model_group};
use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};
use tokio::time::{Duration, sleep};

fn test_config(tmp: &tempfile::TempDir) -> Config {
    let base = tmp.path().to_string_lossy().to_string();
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
            shared_config_dir: format!("{base}/config"),
            skills_dir: format!("{base}/skills"),
            cache_dir: format!("{base}/cache"),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn build_state(provider: Arc<MockModelProvider>) -> (AppState, tempfile::TempDir) {
    init_metrics();
    let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
    db_init::setup_schema(&db).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let storage = StorageService::new(&config);
    let resource_manager = std::sync::Arc::new(
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

    let mut providers: HashMap<String, Arc<dyn frona::inference::provider::ModelProvider>> =
        HashMap::new();
    providers.insert("mock".to_string(), provider);
    let mut groups = HashMap::new();
    groups.insert("test".to_string(), test_model_group());
    let mock_registry = ModelProviderRegistry::for_testing(providers, groups);

    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let prompts_dir = manifest
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("resources/prompts");
    let prompts = frona::agent::prompt::PromptLoader::new(prompts_dir);
    state.prompts = prompts.clone();

    let chat_service = ChatService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        state.agent_service.clone(),
        mock_registry,
        state.storage_service.clone(),
        state.user_service.clone(),
        prompts,
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

    state.init_signal_service();
    state.policy_service.sync_base_policies().await.unwrap();

    (state, tmp)
}

async fn seed_user_and_agent(state: &AppState, user_id: &str, agent_id: &str) {
    SurrealRepo::<User>::new(state.db.clone())
        .create(&User {
            id: user_id.into(),
            handle: frona::handle!("test-user"),
            email: format!("{user_id}@test.com"),
            name: "Test User".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    SurrealRepo::<Agent>::new(state.db.clone())
        .create(&Agent {
            id: agent_id.into(),
            user_id: user_id.into(),
            handle: frona::handle!("channel-agent"),
            name: "Channel Agent".into(),
            description: String::new(),
            model_group: "test".into(),
            enabled: true,
            skills: None,
            sandbox_limits: None,
            max_concurrent_tasks: Some(5),
            avatar: None,
            identity: Default::default(),
            prompt: Some("You are the channel-inbound agent.".into()),
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
}

async fn seed_space_and_chat(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    chat_external_id: &str,
) -> (Space, Chat) {
    let space = Space {
        id: format!("space-{user_id}"),
        user_id: user_id.into(),
        name: "Telegram".into(),
        metadata: BTreeMap::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    SurrealRepo::<Space>::new(state.db.clone())
        .create(&space)
        .await
        .unwrap();

    let now = Utc::now();
    let channel = frona::chat::channel::Channel {
        id: format!("channel-{user_id}"),
        user_id: user_id.into(),
        handle: frona::handle!("telegram"),
        space_id: space.id.clone(),
        provider: "telegram".into(),
        agent_id: agent_id.into(),
        config: std::collections::BTreeMap::new(),
        dispatch_mode: frona::chat::channel::DispatchMode::Message,
        status: frona::chat::channel::ChannelStatus::Disconnected,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        enabled: true,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    use frona::core::repository::Repository;
    SurrealRepo::<frona::chat::channel::Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();

    let mut chat_md = BTreeMap::new();
    chat_md.insert("channel:external_id".into(), json!(chat_external_id));

    let chat = state
        .chat_service
        .create_chat(
            user_id,
            CreateChatRequest {
                space_id: Some(space.id.clone()),
                task_id: None,
                agent_id: agent_id.into(),
                title: Some("Telegram chat".into()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    state
        .chat_service
        .patch_chat_metadata(&chat.id, chat_md)
        .await
        .unwrap();

    let chat = state
        .chat_service
        .find_chat(&chat.id)
        .await
        .unwrap()
        .unwrap();

    (space, chat)
}

async fn post_inbound(state: &AppState, chat_id: &str, content: &str, sender: &str) {
    let mut md = BTreeMap::new();
    md.insert("channel:from_address".into(), json!(sender));
    // `from_address` is the channel-inbound discriminator: the dispatcher
    // uses it to distinguish channel-inbound messages from web-submitted
    // ones (which already have inference fired from `/messages/stream`).
    let msg = Message::builder(chat_id, MessageRole::User, content.into())
        .from_address(sender)
        .metadata(md)
        .build();
    state
        .chat_service
        .persist_inbound_message(&msg)
        .await
        .unwrap();
}

async fn wait_until<F: Fn() -> bool>(predicate: F, max_wait: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < max_wait {
        if predicate() {
            return true;
        }
        sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

#[tokio::test]
async fn dispatcher_invokes_channel_agent_on_inbound_user_message() {
    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "noted".into(),
    )]));
    let (state, _tmp) = build_state(provider.clone()).await;
    seed_user_and_agent(&state, "user-1", "agent-1").await;
    let (_space, chat) = seed_space_and_chat(&state, "user-1", "agent-1", "dm:9999").await;

    frona::chat::channel::spawn_inference_dispatcher(state.clone());

    post_inbound(&state, &chat.id, "hi there", "@alice").await;

    let fired = wait_until(|| provider.calls() >= 1, Duration::from_secs(3)).await;
    assert!(
        fired,
        "channel-agent inference should be invoked on inbound user message (calls = {})",
        provider.calls()
    );
}

#[tokio::test]
async fn dispatcher_skips_messages_in_chats_without_channel_provider() {
    let provider = Arc::new(MockModelProvider::new(vec![]));
    let (state, _tmp) = build_state(provider.clone()).await;
    seed_user_and_agent(&state, "user-1", "agent-1").await;

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".into(),
                title: Some("Plain chat".into()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    frona::chat::channel::spawn_inference_dispatcher(state.clone());
    post_inbound(&state, &chat.id, "hi", "@alice").await;

    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        provider.calls(),
        0,
        "non-channel chats must not trigger the dispatcher"
    );
}

/// **Regression**: a web-submitted user message (created via `/messages/stream`,
/// which has no `from_address`) MUST NOT trigger the channel inbound
/// dispatcher. Otherwise both the stream route handler AND the dispatcher
/// fire inference on the same user turn, producing two parallel agent runs
/// (observed in prod as "ask me 3 questions" returning 6 questions).
#[tokio::test]
async fn dispatcher_skips_web_submitted_messages_in_channel_chats() {
    let provider = Arc::new(MockModelProvider::new(vec![]));
    let (state, _tmp) = build_state(provider.clone()).await;
    seed_user_and_agent(&state, "user-1", "agent-1").await;
    let (_space, chat) = seed_space_and_chat(&state, "user-1", "agent-1", "dm:8888").await;

    frona::chat::channel::spawn_inference_dispatcher(state.clone());

    // Simulates `/messages/stream` - `from_address` is None for web submissions.
    state
        .chat_service
        .create_stream_user_message("user-1", &chat.id, "hi", vec![], None)
        .await
        .unwrap();

    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        provider.calls(),
        0,
        "web-submitted messages (from_address = None) must not fan out to \
         the dispatcher — the stream route already triggers inference"
    );
}

#[tokio::test]
async fn dispatcher_skips_non_user_messages() {
    let provider = Arc::new(MockModelProvider::new(vec![]));
    let (state, _tmp) = build_state(provider.clone()).await;
    seed_user_and_agent(&state, "user-1", "agent-1").await;
    let (_space, chat) = seed_space_and_chat(&state, "user-1", "agent-1", "dm:1111").await;

    frona::chat::channel::spawn_inference_dispatcher(state.clone());

    state
        .chat_service
        .save_system_message("user-1", None, &chat.id, "system note".into())
        .await
        .unwrap();

    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        provider.calls(),
        0,
        "system-role messages must not trigger the dispatcher"
    );
}
