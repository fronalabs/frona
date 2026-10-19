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
        let tool_fixture = helpers::app_state::build(&db).await;
        let tool_manager = tool_fixture.state.tool_manager.clone();
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

    let provider_registry = crate::helpers::test_model_service(providers, model_groups).await;

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
        {
            let mut loaded = frona::core::config::ConfigService::load(
                tempfile::tempdir().unwrap().path().join("config.yaml"),
            )
            .unwrap();
            loaded.config = config.clone();
            frona::core::config::ConfigService::new(loaded).unwrap()
        },
        Some(frona::inference::config::ModelRegistryConfig::empty()),
        storage.clone(),
        metrics_handle,
        resource_manager.clone(),
        crate::helpers::app_state::catalogs(&config),
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

struct StopFixture {
    state: AppState,
    _directory: tempfile::TempDir,
    task: Task,
    mock: Arc<MockModelProvider>,
}

async fn stop_fixture(responses: Vec<MockResponse>, status: TaskStatus) -> StopFixture {
    let mock = Arc::new(MockModelProvider::new(responses));
    let (state, directory) = test_app_state_with_mock(mock.clone()).await;
    seed_agent(&state.db).await;
    seed_user(&state.db).await;
    let mut task = make_task();
    task.status = status;
    state
        .task_executor
        .ensure_task_chat(&mut task)
        .await
        .unwrap();
    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();
    StopFixture {
        state,
        _directory: directory,
        task,
        mock,
    }
}

async fn stop_chat(state: &AppState, chat_id: &str) -> bool {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let user = state
        .user_service
        .find_by_id("user-1")
        .await
        .unwrap()
        .unwrap();
    let token = state
        .token_service
        .create_access_token(&state.keypair_service, &user, "stop-test")
        .await
        .unwrap();
    let response = frona::api::routes::messages::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/chats/{chat_id}/cancel"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["cancelled"]
        .as_bool()
        .unwrap()
}

#[tokio::test]
async fn chat_stop_cancels_the_task_provider_without_releasing_its_barrier() {
    let fixture = stop_fixture(
        vec![MockResponse::Barrier(
            Arc::new(tokio::sync::Barrier::new(2)),
            Box::new(MockResponse::Text("must not finish".into())),
        )],
        TaskStatus::Pending,
    )
    .await;
    let chat_id = fixture.task.chat_id.clone().unwrap();
    let task_id = fixture.task.id.clone();
    let executor = fixture.state.task_executor.clone();
    let run = tokio::spawn(async move {
        executor.run_task(fixture.task).await.unwrap();
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.mock.calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(stop_chat(&fixture.state, &chat_id).await);
    tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.mock.calls(), 1);
    assert_eq!(
        fixture
            .state
            .task_service
            .find_by_id(&task_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Cancelled
    );
}

#[tokio::test]
async fn chat_stop_between_task_turns_prevents_the_next_provider_call() {
    use frona::{chat::broadcast::BroadcastEventKind, inference::tool_loop::InferenceEventKind};
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let fixture = stop_fixture(
        vec![MockResponse::Barrier(
            gate.clone(),
            Box::new(MockResponse::Text("first turn".into())),
        )],
        TaskStatus::Pending,
    )
    .await;
    let chat_id = fixture.task.chat_id.clone().unwrap();
    let task_id = fixture.task.id.clone();
    let mut events = fixture.state.broadcast_service.subscribe_raw();
    let mut run = Box::pin(fixture.state.task_executor.run_task(fixture.task));
    let release = tokio::spawn(async move {
        gate.wait().await;
    });
    // Drive the production executor only until Done. Its following lifecycle
    // DB query yields, so the biased select leaves it unpolled during Stop.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                biased;
                event = events.recv() => {
                    if matches!(event.unwrap().kind, BroadcastEventKind::Inference(InferenceEventKind::Done { .. })) { break; }
                }
                result = &mut run => panic!("task ended before Stop: {result:?}"),
            }
        }
    }).await.unwrap();
    release.await.unwrap();
    assert!(stop_chat(&fixture.state, &chat_id).await);
    tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.mock.calls(), 1);
    assert_eq!(
        fixture
            .state
            .task_service
            .find_by_id(&task_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Cancelled
    );
}

#[tokio::test]
async fn chat_stop_preserves_terminal_tasks_and_cancels_task_chat_replies() {
    for status in [
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        let fixture = stop_fixture(vec![], status).await;
        let chat_id = fixture.task.chat_id.clone().unwrap();
        let before = fixture
            .state
            .task_service
            .find_by_id(&fixture.task.id)
            .await
            .unwrap()
            .unwrap();
        assert!(!stop_chat(&fixture.state, &chat_id).await);
        // A normal chat response may run on a terminal task's chat.
        let token = fixture.state.active_sessions.register(&chat_id).await;
        assert!(stop_chat(&fixture.state, &chat_id).await);
        assert!(token.is_cancelled());
        let after = fixture
            .state
            .task_service
            .find_by_id(&fixture.task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
    }
}

fn install_question_tool(fixture: &StopFixture) {
    let tools = fixture._directory.path().join("config/prompts/tools");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../resources/prompts/tools/ask_user_question.md"
        ),
        tools.join("ask_user_question.md"),
    )
    .unwrap();
}

async fn paused_question(
    fixture: &StopFixture,
    chat_id: &str,
) -> frona::inference::tool_call::ToolCall {
    use frona::chat::message::models::{Message, MessageStatus};
    use frona::inference::hitl::{Hitl, HitlRequest};
    use frona::inference::tool_call::{ToolCall, ToolStatus};

    install_question_tool(fixture);
    let response = fixture
        .state
        .chat_service
        .create_executing_agent_message(chat_id, "test-agent")
        .await
        .unwrap();
    let messages: SurrealRepo<Message> = SurrealRepo::new(fixture.state.db.clone());
    let mut message = messages.find_by_id(&response.id).await.unwrap().unwrap();
    message.status = Some(MessageStatus::Paused);
    message.content = "Before approval".into();
    messages.update(&message).await.unwrap();
    let call = ToolCall {
        id: frona::core::repository::new_id(),
        chat_id: chat_id.into(),
        message_id: message.id.clone(),
        turn: 0,
        provider_call_id: "question".into(),
        name: "ask_user_question".into(),
        arguments: serde_json::json!({"question": "Continue?"}),
        result: String::new(),
        success: false,
        duration_ms: 0,
        hitl: Some(Hitl {
            prompt: "Continue?".into(),
            url: format!("/chats/{chat_id}"),
            request: HitlRequest::Question {
                options: vec!["yes".into()],
            },
            status: ToolStatus::Pending,
            response: None,
            delivery: None,
        }),
        task_event: None,
        system_prompt: None,
        description: None,
        turn_text: None,
        signature: None,
        turn_reasoning: None,
        created_at: Utc::now(),
    };
    let calls: SurrealRepo<ToolCall> = SurrealRepo::new(fixture.state.db.clone());
    calls.create(&call).await.unwrap();
    call
}

#[tokio::test]
async fn chat_stop_before_approved_task_starts_preserves_its_response() {
    use frona::{
        chat::message::models::MessageStatus,
        inference::hitl::{HitlResponse, ResolveOutcome},
    };
    let fixture = stop_fixture(vec![], TaskStatus::InProgress).await;
    let chat_id = fixture.task.chat_id.as_deref().unwrap();
    let question = paused_question(&fixture, chat_id).await;
    let message_id = question.message_id;
    let call_id = question.id;
    let outcome = fixture
        .state
        .harness
        .resolve_and_resume(&call_id, HitlResponse::Choice("yes".into()))
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ResolveOutcome::Resolved {
            should_resume: true,
            ..
        }
    ));
    // This is the real approval/startup gap: the response is Executing, but
    // the spawned task has not registered its lifetime token yet.
    assert!(stop_chat(&fixture.state, chat_id).await);
    fixture
        .state
        .task_executor
        .run_task_by_id(&fixture.task.id)
        .await
        .unwrap();
    assert_eq!(fixture.mock.calls(), 0);
    let message = fixture
        .state
        .chat_service
        .find_message(&message_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.status, Some(MessageStatus::Cancelled));
    assert_eq!(message.content, "Before approval");
    assert_eq!(
        fixture
            .state
            .task_service
            .find_by_id(&fixture.task.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Cancelled
    );
}

#[tokio::test]
async fn chat_stop_before_registered_chat_resume_preserves_its_response() {
    use frona::chat::message::models::MessageStatus;
    let fixture = stop_fixture(vec![], TaskStatus::Completed).await;
    let chat_id = fixture.task.chat_id.as_deref().unwrap();
    let question = paused_question(&fixture, chat_id).await;
    let message_id = question.message_id;
    let call_id = question.id;
    fixture
        .state
        .harness
        .resolve_and_resume(
            &call_id,
            frona::inference::hitl::HitlResponse::Choice("yes".into()),
        )
        .await
        .unwrap();
    let token = fixture.state.active_sessions.register(chat_id).await;
    assert!(stop_chat(&fixture.state, chat_id).await);
    fixture
        .state
        .harness
        .resume_with_cancel("user-1", chat_id, &message_id, token)
        .await
        .unwrap();
    assert_eq!(fixture.mock.calls(), 0);
    let message = fixture
        .state
        .chat_service
        .find_message(&message_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.status, Some(MessageStatus::Cancelled));
    assert_eq!(message.content, "Before approval");
}

#[tokio::test]
async fn approved_task_continues_after_the_previous_invocation_finishes() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use frona::{chat::broadcast::BroadcastEventKind, inference::tool_loop::InferenceEventKind};
    use tower::ServiceExt;

    let fixture = stop_fixture(
        vec![
            MockResponse::ToolCalls(vec![(
                "question".into(),
                "ask_user_question".into(),
                serde_json::json!({"question": "Continue?", "options": ["yes"]}),
            )]),
            MockResponse::Barrier(
                Arc::new(tokio::sync::Barrier::new(2)),
                Box::new(MockResponse::Text("must not finish".into())),
            ),
        ],
        TaskStatus::Pending,
    )
    .await;
    install_question_tool(&fixture);
    fixture
        .state
        .policy_service
        .sync_base_policies()
        .await
        .unwrap();
    let chat_id = fixture.task.chat_id.as_deref().unwrap();
    let mut events = fixture.state.broadcast_service.subscribe_raw();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.state.task_executor.run_task(fixture.task.clone()),
    )
    .await
    .expect("question must pause the task before a second provider call")
    .unwrap();
    let call_id = loop {
        let event = events
            .try_recv()
            .expect("task must publish its approval pause");
        if let BroadcastEventKind::Inference(InferenceEventKind::Paused { message, .. }) =
            event.kind
        {
            break message.tool_calls[0].id.clone();
        }
    };
    let user = fixture
        .state
        .user_service
        .find_by_id("user-1")
        .await
        .unwrap()
        .unwrap();
    let token = fixture
        .state
        .token_service
        .create_access_token(&fixture.state.keypair_service, &user, "approval-test")
        .await
        .unwrap();
    let response = frona::api::routes::messages::router().with_state(fixture.state.clone()).oneshot(
        Request::builder().method("POST").uri(format!("/api/chats/{chat_id}/tool-calls/resolve"))
            .header("authorization", format!("Bearer {token}")).header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"resolutions": [{"tool_call_id": call_id, "hitl_response": {"type": "Choice", "data": "yes"}}]}).to_string())).unwrap()
    ).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    wait_for_provider_calls(&fixture.mock, 2).await;
    assert_eq!(fixture.state.task_executor.active_count().await, 1);
    assert!(stop_chat(&fixture.state, chat_id).await);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.state.task_executor.active_count().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.mock.calls(), 2);
}

#[tokio::test]
async fn queued_task_resume_waits_for_execution_and_honours_stop() {
    let fixture = stop_fixture(
        vec![MockResponse::Barrier(
            Arc::new(tokio::sync::Barrier::new(2)),
            Box::new(MockResponse::Text("must not finish".into())),
        )],
        TaskStatus::Pending,
    )
    .await;
    let executor = fixture.state.task_executor.clone();
    let task = fixture.task.clone();
    let run = tokio::spawn(async move { executor.run_task(task).await.unwrap() });
    wait_for_provider_calls(&fixture.mock, 1).await;

    let mut resume = Box::pin(
        fixture
            .state
            .task_executor
            .resume_task_by_id(&fixture.task.id),
    );
    // Start the continuation while the real executor is held at inference.
    // Stop must remain independent of the queued continuation's execution lock.
    std::future::poll_fn(|cx| {
        assert!(resume.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert!(stop_chat(&fixture.state, fixture.task.chat_id.as_deref().unwrap()).await);
    tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), resume)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.mock.calls(), 1);
    assert_eq!(
        fixture
            .state
            .task_service
            .find_by_id(&fixture.task.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Cancelled,
    );
}

async fn wait_for_provider_calls(mock: &MockModelProvider, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while mock.calls() < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn execution_lock_skips_duplicates_allows_other_tasks_and_releases_on_abort() {
    let fixture = stop_fixture(
        (0..3)
            .map(|_| {
                MockResponse::Barrier(
                    Arc::new(tokio::sync::Barrier::new(2)),
                    Box::new(MockResponse::Text("must not finish".into())),
                )
            })
            .collect(),
        TaskStatus::Pending,
    )
    .await;
    let executor = fixture.state.task_executor.clone();
    let task = fixture.task.clone();
    let first = tokio::spawn(async move { executor.run_task(task).await.unwrap() });
    wait_for_provider_calls(&fixture.mock, 1).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.state.task_executor.run_task(fixture.task.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        fixture.mock.calls(),
        1,
        "duplicate must not start inference"
    );

    let mut other = make_task();
    fixture
        .state
        .task_executor
        .ensure_task_chat(&mut other)
        .await
        .unwrap();
    let repo: SurrealRepo<Task> = SurrealRepo::new(fixture.state.db.clone());
    repo.create(&other).await.unwrap();
    let other_id = other.id.clone();
    let executor = fixture.state.task_executor.clone();
    let second = tokio::spawn(async move { executor.run_task(other).await.unwrap() });
    wait_for_provider_calls(&fixture.mock, 2).await;
    assert_eq!(fixture.state.task_executor.active_count().await, 2);

    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let executor = fixture.state.task_executor.clone();
    let task_id = fixture.task.id.clone();
    let resumed = tokio::spawn(async move { executor.resume_task_by_id(&task_id).await.unwrap() });
    wait_for_provider_calls(&fixture.mock, 3).await;
    assert_eq!(fixture.state.task_executor.active_count().await, 2);
    assert!(stop_chat(&fixture.state, fixture.task.chat_id.as_deref().unwrap()).await);
    assert!(fixture.state.task_executor.cancel_task(&other_id).await);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        resumed.await.unwrap();
        second.await.unwrap();
        while fixture.state.task_executor.active_count().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn chat_stop_during_resumed_provider_preserves_existing_response_text() {
    use frona::chat::message::models::MessageStatus;
    let fixture = stop_fixture(
        vec![MockResponse::Barrier(
            Arc::new(tokio::sync::Barrier::new(2)),
            Box::new(MockResponse::Text("must not finish".into())),
        )],
        TaskStatus::Completed,
    )
    .await;
    let chat_id = fixture.task.chat_id.clone().unwrap();
    let question = paused_question(&fixture, &chat_id).await;
    fixture
        .state
        .harness
        .resolve_and_resume(
            &question.id,
            frona::inference::hitl::HitlResponse::Choice("yes".into()),
        )
        .await
        .unwrap();
    let token = fixture.state.active_sessions.register(&chat_id).await;
    let harness = fixture.state.harness.clone();
    let message_id = question.message_id;
    let resume_chat = chat_id.clone();
    let resume_message = message_id.clone();
    let resume = tokio::spawn(async move {
        harness
            .resume_with_cancel("user-1", &resume_chat, &resume_message, token)
            .await
            .unwrap();
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.mock.calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(stop_chat(&fixture.state, &chat_id).await);
    tokio::time::timeout(std::time::Duration::from_secs(5), resume)
        .await
        .unwrap()
        .unwrap();
    let message = fixture
        .state
        .chat_service
        .find_message(&message_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.status, Some(MessageStatus::Cancelled));
    assert_eq!(message.content, "Before approval");
}
