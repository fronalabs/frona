//! Integration test: execute a task end-to-end and verify every SSE event
//! that the frontend would receive.

mod helpers;

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use frona::agent::service::AgentService;
use frona::agent::task::executor::TaskExecutor;
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::core::config::Config;
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::storage::StorageService;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};
use tokio::sync::mpsc;

use helpers::{MockModelProvider, MockResponse, SseFrame, drain_sse_frames, test_model_group};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

/// Build an `AppState` whose provider registry contains a mock provider
/// and a model group named "primary" that resolves to it.
async fn test_app_state_with_mock(mock: Arc<MockModelProvider>) -> (AppState, tempfile::TempDir) {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_string_lossy().to_string();

    let config = Config {
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
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    };

    let storage = StorageService::new(&config);
    let resource_manager = Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let user_service = frona::auth::UserService::new(SurrealRepo::new(db.clone()), &config.cache);
    let policy_service = {
        let schema = frona::policy::schema::build_schema();
        let repo: std::sync::Arc<dyn frona::policy::repository::PolicyRepository> =
            std::sync::Arc::new(SurrealRepo::<frona::policy::models::Policy>::new(
                db.clone(),
            ));
        let tool_manager = std::sync::Arc::new(frona::tool::manager::ToolManager::new(false));
        let storage = frona::storage::StorageService::new(&config);
        frona::policy::service::PolicyService::new(
            repo,
            schema,
            tool_manager,
            storage,
            user_service.clone(),
        )
    };
    let agent_service = AgentService::new(
        SurrealRepo::new(db.clone()),
        &config.cache,
        resource_manager.clone(),
        policy_service,
        user_service.clone(),
    );

    let mut providers: HashMap<String, Arc<dyn frona::inference::provider::ModelProvider>> =
        HashMap::new();
    providers.insert("mock".to_string(), mock);

    let mut model_groups = HashMap::new();
    model_groups.insert("primary".to_string(), test_model_group());

    let provider_registry =
        frona::inference::registry::ModelProviderRegistry::for_testing(providers, model_groups);

    let user_service = frona::auth::UserService::new(SurrealRepo::new(db.clone()), &config.cache);
    let prompt_loader = frona::agent::prompt::PromptLoader::new(format!("{base}/prompts"));

    let provider_registry_arc = Arc::new(provider_registry.clone());
    let memory_service: Arc<dyn frona::memory::service::MemoryService> =
        Arc::new(frona::memory::basic::BasicMemoryService::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            provider_registry_arc,
            prompt_loader.clone(),
            helpers::test_usage_service(&db),
            frona::core::config::MemoryConfig::default(),
        ));

    let metrics_handle = frona::core::metrics::setup_metrics_recorder();
    let mut state = AppState::new(
        db.clone(),
        &config,
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage.clone(),
        metrics_handle,
        resource_manager.clone(),
    );
    // Must reuse `state.broadcast_service` - a fresh BroadcastService here
    // would disconnect events fired inside ChatService from SSE sessions
    // registered against state.broadcast_service.
    let chat_service = frona::chat::service::ChatService::new(
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        SurrealRepo::new(db.clone()),
        agent_service.clone(),
        provider_registry,
        storage.clone(),
        user_service,
        prompt_loader.clone(),
        state.broadcast_service.clone(),
        state.presign_service.clone(),
        state.usage_service.clone(),
    );
    // Replace the chat_service with our version that has the mock provider.
    state.chat_service = chat_service.clone();
    // Replace the agent_service so chat_service in state shares the same
    // underlying repo as the one above.
    state.agent_service = agent_service.clone();
    state.harness = Arc::new(frona::agent::harness::Harness::new(
        chat_service,
        state.user_service.clone(),
        state.storage_service.clone(),
        agent_service,
        memory_service,
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

    (state, tmp)
}

fn make_task() -> Task {
    let now = Utc::now();
    Task {
        id: frona::core::repository::new_id(),
        user_id: "user-1".to_string(),
        agent_id: "test-agent".to_string(),
        space_id: None,
        chat_id: None,
        title: "Test task".to_string(),
        description: "Say hello".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct {
            source_chat_id: None,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        quarantined: false,
        result_schema: None,
        result_description: None,
        created_at: now,
        updated_at: now,
    }
}

/// Create a minimal agent in the DB so that `ChatSessionContext::build` can
/// resolve it.
async fn seed_agent(db: &Surreal<Db>) {
    let agent = frona::agent::models::Agent {
        id: "test-agent".to_string(),
        user_id: "user-1".to_string(),
        handle: frona::handle!("test-agent"),
        name: "Test Agent".to_string(),
        description: String::new(),
        model_group: "primary".to_string(),
        enabled: true,
        skills: None,
        sandbox_limits: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: Default::default(),
        prompt: Some("You are a test agent. Do what the user asks.".to_string()),
        heartbeat_interval: None,
        next_heartbeat_at: None,
        heartbeat_chat_id: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let repo: SurrealRepo<frona::agent::models::Agent> = SurrealRepo::new(db.clone());
    repo.create(&agent).await.unwrap();
}

/// Create a minimal user in the DB.
async fn seed_user(db: &Surreal<Db>) {
    let user = frona::auth::User {
        id: "user-1".to_string(),
        handle: frona::handle!("testuser"),
        email: "test@test.com".to_string(),
        name: "Test".to_string(),
        password_hash: String::new(),
        timezone: None,
        groups: Vec::new(),
        deactivated_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let repo: SurrealRepo<frona::auth::User> = SurrealRepo::new(db.clone());
    repo.create(&user).await.unwrap();
}

/// Execute a task that produces a simple text response and verify the
/// complete sequence of SSE events the frontend would receive.
#[tokio::test]
async fn task_execution_emits_expected_sse_events() {
    helpers::init_metrics();

    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "Hello from the task!".to_string(),
    )]));

    let (state, _tmp) = test_app_state_with_mock(mock).await;
    seed_agent(&state.db).await;
    seed_user(&state.db).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    state.broadcast_service.register_session("user-1", tx).await;

    let task = make_task();
    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    // Execute the task in the background while we poll for status.
    let task_id = task.id.clone();
    let executor = Arc::new(TaskExecutor::new(state.harness.clone()));
    let exec_for_spawn = executor.clone();
    tokio::spawn(async move {
        let _ = exec_for_spawn.run_task(task).await;
    });

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(t) = repo.find_by_id(&task_id).await.unwrap()
            && matches!(
                t.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        {
            break;
        }
    }

    // Give the dispatcher a moment to route remaining events.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let all_frames: Vec<SseFrame> = drain_sse_frames(&mut rx).await;
    let all_event_names: Vec<&str> = all_frames.iter().map(|f| f.event.as_str()).collect();

    println!("SSE events received: {all_event_names:?}");
    for frame in &all_frames {
        println!("  {}: {}", frame.event, frame.data);
    }

    assert_eq!(
        all_frames.first().map(|frame| frame.event.as_str()),
        Some("activity_changed"),
        "Task activity should start before task progress"
    );
    assert_eq!(
        all_frames.last().map(|frame| frame.event.as_str()),
        Some("activity_changed"),
        "Task activity should finish after the terminal task update"
    );
    assert_eq!(
        all_frames
            .iter()
            .filter(|frame| frame.event == "activity_changed")
            .count(),
        2,
        "Task execution should emit one start and one finish activity wakeup"
    );

    // `entity_updated` belongs to the channel watcher. `activity_changed`
    // brackets the task lifecycle and is asserted separately above.
    let frames: Vec<SseFrame> = all_frames
        .into_iter()
        .filter(|f| f.event != "entity_updated" && f.event != "activity_changed")
        .collect();
    let event_names: Vec<&str> = frames.iter().map(|f| f.event.as_str()).collect();

    // Expected sequence (first turn + final task_update):
    //   task_update(inprogress) → chat_message → token → inference_done → ... → task_update
    assert!(
        event_names.len() >= 4,
        "Expected at least 4 events, got {}: {event_names:?}",
        event_names.len()
    );

    // First event: task_update with status=inprogress
    assert_eq!(
        event_names[0], "task_update",
        "First event should be task_update"
    );
    assert_eq!(
        frames[0].data["status"].as_str().unwrap(),
        "inprogress",
        "First task_update should be inprogress"
    );

    assert_eq!(
        event_names[1], "chat_message",
        "Second event should be chat_message"
    );

    assert_eq!(
        event_names[2], "inference_start",
        "Third event should be inference_start"
    );

    assert_eq!(event_names[3], "token", "Fourth event should be token");
    assert_eq!(
        frames[3].data["content"].as_str().unwrap(),
        "Hello from the task!",
        "Token should carry the response text"
    );

    assert_eq!(
        event_names[4], "usage_recorded",
        "Fifth event should be usage_recorded"
    );
    assert!(
        frames[4].data["model_ref"].is_string(),
        "usage_recorded should carry model_ref"
    );

    // `complete_agent_message` no longer fires `chat_message` for streaming
    // completions - `inference_done` is the canonical signal.
    assert_eq!(
        event_names[5], "inference_done",
        "Sixth event should be inference_done"
    );
    let message = &frames[5].data["message"];
    assert!(
        message.is_object(),
        "inference_done should carry a message object"
    );
    assert_eq!(
        message["content"].as_str().unwrap(),
        "Hello from the task!",
        "inference_done message should contain the response text"
    );
    assert_eq!(
        message["status"].as_str().unwrap(),
        "completed",
        "inference_done message status should be completed"
    );

    // Last event: task_update with terminal status
    let last = frames.last().unwrap();
    assert_eq!(
        last.event, "task_update",
        "Last event should be task_update"
    );
}

/// E2E for the delegate→deliver path. Verifies BOTH:
/// - SSE: parent chat receives a `chat_message` for the TaskCompletion.
/// - DB: TaskCompletion row persisted in the parent (source) chat.
#[tokio::test]
async fn delegation_delivers_task_result_to_parent_chat() {
    helpers::init_metrics();

    let mock = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "Researcher reports: 42.".to_string(),
    )]));

    let (state, _tmp) = test_app_state_with_mock(mock).await;
    seed_agent(&state.db).await;
    seed_user(&state.db).await;

    let parent_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "test-agent".to_string(),
                title: Some("Parent".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();
    state
        .chat_service
        .create_stream_user_message(
            "user-1",
            &parent_chat.id,
            "Ask the researcher: what is the answer to life?",
            vec![],
            None,
        )
        .await
        .unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    state.broadcast_service.register_session("user-1", tx).await;

    let now = Utc::now();
    let task = Task {
        id: frona::core::repository::new_id(),
        user_id: "user-1".to_string(),
        agent_id: "test-agent".to_string(),
        space_id: None,
        chat_id: None,
        title: "Researcher task".to_string(),
        description: "What is the answer to life?".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Delegation {
            source_agent_id: "test-agent".to_string(),
            source_chat_id: parent_chat.id.clone(),
            resume_parent: false,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        quarantined: false,
        result_schema: None,
        result_description: None,
        created_at: now,
        updated_at: now,
    };
    let task_repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    task_repo.create(&task).await.unwrap();

    let task_id = task.id.clone();
    let executor = Arc::new(TaskExecutor::new(state.harness.clone()));
    let exec_for_spawn = executor.clone();
    tokio::spawn(async move {
        let _ = exec_for_spawn.run_task(task).await;
    });

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(t) = task_repo.find_by_id(&task_id).await.unwrap()
            && matches!(
                t.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        {
            break;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let parent_msgs = state
        .chat_service
        .get_stored_messages(&parent_chat.id)
        .await
        .unwrap();
    let task_completion_msgs: Vec<_> = parent_msgs
        .iter()
        .filter(|m| m.role == frona::chat::message::models::MessageRole::TaskCompletion)
        .collect();
    assert_eq!(
        task_completion_msgs.len(),
        1,
        "Parent chat must contain exactly one TaskCompletion message - got: {parent_msgs:?}"
    );
    let task_completion = task_completion_msgs[0];
    assert_eq!(
        task_completion.chat_id, parent_chat.id,
        "TaskCompletion message must be saved in the parent (source) chat, not the task chat",
    );

    let parent_agent_msgs: Vec<_> = parent_msgs
        .iter()
        .filter(|m| m.role == frona::chat::message::models::MessageRole::Agent)
        .collect();
    assert!(
        parent_agent_msgs.is_empty(),
        "Parent chat must not contain Agent messages from the task's inference. Leaked: {parent_agent_msgs:?}"
    );

    // Find the task's own chat (created by `ensure_task_chat`) and verify
    // the child agent's response landed there, not in the parent.
    let updated_task = task_repo.find_by_id(&task_id).await.unwrap().unwrap();
    let task_chat_id = updated_task
        .chat_id
        .as_ref()
        .expect("task should have its own chat after execution");
    assert_ne!(
        task_chat_id, &parent_chat.id,
        "Task chat must be distinct from parent chat",
    );
    let task_chat_msgs = state
        .chat_service
        .get_stored_messages(task_chat_id)
        .await
        .unwrap();
    let task_chat_agent_msgs: Vec<_> = task_chat_msgs
        .iter()
        .filter(|m| {
            m.role == frona::chat::message::models::MessageRole::Agent && !m.content.is_empty()
        })
        .collect();
    assert!(
        !task_chat_agent_msgs.is_empty(),
        "Task chat must contain at least one Agent message with the child agent's response. Got: {task_chat_msgs:?}"
    );
    assert!(
        task_chat_agent_msgs
            .iter()
            .any(|m| m.content.contains("Researcher reports")),
        "Task chat should contain the mocked child response. Got: {task_chat_agent_msgs:?}"
    );

    let frames = drain_sse_frames(&mut rx).await;
    let delivery_frame = frames.iter().find(|f| {
        f.event == "chat_message"
            && f.data["chat_id"].as_str() == Some(parent_chat.id.as_str())
            && f.data["message"]["role"].as_str() == Some("taskcompletion")
    });
    let delivery_frame = delivery_frame.unwrap_or_else(|| {
        panic!(
            "Expected a `chat_message` SSE for the TaskCompletion in the parent chat. Frames seen: {:?}",
            frames.iter().map(|f| (&f.event, &f.data)).collect::<Vec<_>>()
        )
    });

    // SSE id must equal DB row id - otherwise live view and refresh diverge.
    let sse_msg_id = delivery_frame.data["message"]["id"]
        .as_str()
        .expect("SSE chat_message must carry a message.id");
    assert_eq!(
        sse_msg_id, task_completion.id,
        "SSE message.id must equal the DB row id - otherwise refresh diverges from live view"
    );
    let sse_msg_chat_id = delivery_frame.data["message"]["chat_id"]
        .as_str()
        .expect("SSE chat_message must carry message.chat_id");
    assert_eq!(
        sse_msg_chat_id, parent_chat.id,
        "SSE message.chat_id must point at the parent chat - otherwise the event is dispatched to the wrong chat store"
    );

    // And `entity_updated` for the watcher.
    let entity_frame = frames.iter().find(|f| {
        f.event == "entity_updated"
            && f.data["table"].as_str() == Some("message")
            && f.data["record_id"].as_str() == Some(task_completion.id.as_str())
    });
    assert!(
        entity_frame.is_some(),
        "Expected an `entity_updated` SSE for the TaskCompletion message id. Frames seen: {:?}",
        frames
            .iter()
            .map(|f| (&f.event, &f.data))
            .collect::<Vec<_>>()
    );
}
