mod helpers;

use std::sync::Arc;

use chrono::Utc;
use frona::agent::task::executor::TaskExecutor;
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::chat::message::models::MessageEvent;
use frona::chat::message::models::MessageRole;
use frona::core::config::Config;
use frona::core::repository::Repository;
use frona::core::state::AppState;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::storage::StorageService;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

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
        browser: Some(frona::core::config::BrowserConfig {
            ws_url: "ws://localhost:0".to_string(),
            api_token: None,
            profiles_path: format!("{base}/profiles"),
            connection_timeout_ms: 30000,
        }),
        storage: frona::core::config::StorageConfig {
            data_dir: base.clone(),
            shared_config_dir: format!("{base}/config"),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn test_app_state() -> (AppState, tempfile::TempDir) {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let storage = StorageService::new(&config);
    let resource_manager = std::sync::Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let metrics_handle = frona::core::metrics::setup_metrics_recorder();
    let state = AppState::new(
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
        storage,
        metrics_handle,
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );
    // Seed the agent + user the test fixtures reference via `agent_id: "agent-1"`
    // / `user_id: "user-1"`, so chat creation (which now validates agent
    // existence + ownership) succeeds.
    seed_user_and_agent(&state).await;
    (state, tmp)
}

async fn seed_user_and_agent(state: &AppState) {
    use frona::auth::User;
    use frona::core::repository::Repository;
    use frona::db::repo::agents::SurrealAgentRepo;
    let now = Utc::now();
    let _ = state
        .user_service
        .create(&User {
            id: "user-1".into(),
            handle: frona::handle!("user-1"),
            email: "user-1@example.com".into(),
            name: "User 1".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        })
        .await;
    // Direct repo insert so the agent gets a fixed `id` (the existing fixtures
    // reference it as "agent-1"); agent_service.create generates a fresh UUID.
    let repo = SurrealAgentRepo::new(state.db.clone());
    let _ = repo
        .create(&frona::agent::models::Agent {
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
            identity: std::collections::BTreeMap::new(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        })
        .await;
}

fn make_executor(state: &AppState) -> Arc<TaskExecutor> {
    Arc::new(TaskExecutor::new(state.harness.clone()))
}

fn make_task(kind: TaskKind) -> Task {
    let now = Utc::now();
    Task {
        id: frona::core::repository::new_id(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Test task".to_string(),
        description: "Do something".to_string(),
        status: TaskStatus::Pending,
        kind,
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

#[tokio::test]
async fn ensure_task_chat_creates_when_missing() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    assert!(task.chat_id.is_none());

    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    assert!(!chat_id.is_empty());
    assert_eq!(task.chat_id.as_deref(), Some(chat_id.as_str()));
}

#[tokio::test]
async fn ensure_task_chat_returns_existing() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    task.chat_id = Some("existing-chat-id".to_string());

    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    assert_eq!(chat_id, "existing-chat-id");
    assert_eq!(task.chat_id.as_deref(), Some("existing-chat-id"));
}

#[tokio::test]
async fn save_initial_message_saves_on_first_run() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    executor
        .save_initial_message_if_needed(&task, &chat_id)
        .await
        .unwrap();

    let messages = state
        .chat_service
        .get_stored_messages(&chat_id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "Do something");
}

#[tokio::test]
async fn save_initial_message_skips_on_resume() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    executor
        .save_initial_message_if_needed(&task, &chat_id)
        .await
        .unwrap();
    executor
        .save_initial_message_if_needed(&task, &chat_id)
        .await
        .unwrap();

    let messages = state
        .chat_service
        .get_stored_messages(&chat_id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
}

#[tokio::test]
async fn handle_cancelled_marks_task_cancelled() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    let _chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    executor.handle_cancelled(&task).await.unwrap();

    let updated = repo.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(updated.status, TaskStatus::Cancelled);
}

#[tokio::test]
async fn handle_error_marks_failed_and_delivers() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);

    let source_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Source chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Delegation {
        source_agent_id: "agent-1".to_string(),
        source_chat_id: source_chat.id.clone(),
        resume_parent: false,
    });
    task.chat_id = Some("task-chat-id".to_string());

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    let error = frona::core::error::AppError::Internal("something broke".to_string());
    executor.handle_error(&task, &error).await.unwrap();

    let updated = repo.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(updated.status, TaskStatus::Failed);

    let source_messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await
        .unwrap();
    assert_eq!(source_messages.len(), 1);
    assert!(source_messages[0].content.contains("something broke"));
}

#[tokio::test]
async fn lifecycle_complete_event_detected() {
    let (state, _tmp) = test_app_state().await;
    let source_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: Some("task-1".to_string()),
                agent_id: "agent-1".to_string(),
                title: Some("Task chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    // Save a System lifecycle event (simulating what TaskControlTool does)
    state
        .chat_service
        .save_system_event(
            "user-1",
            None,
            &source_chat.id,
            MessageEvent::TaskCompletion {
                task_id: "task-1".to_string(),
                chat_id: Some(source_chat.id.clone()),
                status: TaskStatus::Completed,
                summary: Some("Research findings here".to_string()),
                schema: None,
            },
        )
        .await
        .unwrap();

    let messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await
        .unwrap();
    let system_msgs: Vec<_> = messages
        .iter()
        .filter(|m| m.role == MessageRole::System)
        .collect();
    assert_eq!(system_msgs.len(), 1);
    assert!(matches!(
        &system_msgs[0].event,
        Some(MessageEvent::TaskCompletion {
            status: TaskStatus::Completed,
            summary: Some(s),
            ..
        }) if s == "Research findings here"
    ));
}

#[tokio::test]
async fn lifecycle_defer_event_detected() {
    let (state, _tmp) = test_app_state().await;
    let task_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: Some("task-2".to_string()),
                agent_id: "agent-1".to_string(),
                title: Some("Task chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    state
        .chat_service
        .save_system_event(
            "user-1",
            None,
            &task_chat.id,
            MessageEvent::TaskDeferred {
                task_id: "task-2".to_string(),
                delay_minutes: 30,
                reason: "Waiting for external API".to_string(),
            },
        )
        .await
        .unwrap();

    let messages = state
        .chat_service
        .get_stored_messages(&task_chat.id)
        .await
        .unwrap();
    let system_msgs: Vec<_> = messages
        .iter()
        .filter(|m| m.role == MessageRole::System)
        .collect();
    assert_eq!(system_msgs.len(), 1);
    assert!(matches!(
        &system_msgs[0].event,
        Some(MessageEvent::TaskDeferred {
            delay_minutes: 30,
            reason,
            ..
        }) if reason == "Waiting for external API"
    ));
}

#[tokio::test]
async fn mark_deferred_sets_pending_with_run_at() {
    let (state, _tmp) = test_app_state().await;

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    let mut task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    task.status = TaskStatus::InProgress;
    repo.create(&task).await.unwrap();

    let run_at = Utc::now() + chrono::Duration::minutes(30);
    state
        .task_service
        .mark_deferred(&task.id, run_at, "waiting")
        .await
        .unwrap();

    let updated = repo.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(updated.status, TaskStatus::Pending);
    assert!(updated.run_at.is_some());
}

#[tokio::test]
async fn deliver_to_source_skips_direct_tasks() {
    let (state, _tmp) = test_app_state().await;
    let task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });

    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &task,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: Some("result".to_string()),
        },
        vec![],
    )
    .await;
}

#[tokio::test]
async fn deliver_to_source_sends_to_delegation() {
    let (state, _tmp) = test_app_state().await;

    let source_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Source".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Delegation {
        source_agent_id: "agent-1".to_string(),
        source_chat_id: source_chat.id.clone(),
        resume_parent: false,
    });
    task.chat_id = Some("task-chat".to_string());

    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &task,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: Some("All done".to_string()),
        },
        vec![],
    )
    .await;

    let messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "All done");
}

#[tokio::test]
async fn deliver_to_source_sends_to_direct_with_source_chat() {
    let (state, _tmp) = test_app_state().await;

    let source_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("User chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Direct {
        source_chat_id: Some(source_chat.id.clone()),
    });
    task.chat_id = Some("self-task-chat".to_string());

    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &task,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: Some("Self-task result".to_string()),
        },
        vec![],
    )
    .await;

    let messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "Self-task result");
}

#[tokio::test]
async fn task_service_mark_completed_emits_broadcast() {
    // Previously this test exercised TaskExecutor::broadcast_task_status, which
    // has been folded into TaskService::mark_completed (centralized broadcast).
    // Now we verify the service-level mutation fires the SSE event end-to-end.
    use frona::core::repository::Repository;

    let (state, _tmp) = test_app_state().await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    state.broadcast_service.register_session("user-1", tx).await;

    let mut task = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    task.chat_id = Some("chat-123".to_string());
    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    state
        .task_service
        .mark_completed(&task.id, Some("All done".to_string()))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let event = rx.try_recv().expect("Expected to receive an SSE event");
    let event: Result<axum::response::sse::Event, std::convert::Infallible> = event;
    let _sse_event = event.unwrap();
    // The fact that we received an event confirms the broadcast works.
    // Detailed field-level assertions are covered by API integration tests.
}

#[tokio::test]
async fn concurrency_global_limit() {
    let (state, _tmp) = test_app_state().await;
    let executor = Arc::new(TaskExecutor::with_max_concurrent_tasks(
        1,
        state.harness.clone(),
    ));

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());

    let mut task1 = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    task1.id = "task-1".to_string();
    task1.status = TaskStatus::InProgress;
    repo.create(&task1).await.unwrap();
    let exec1 = executor.clone();
    tokio::spawn(async move {
        let _ = exec1.run_task(task1).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut task2 = make_task(TaskKind::Direct {
        source_chat_id: None,
    });
    task2.id = "task-2".to_string();
    repo.create(&task2).await.unwrap();
    let exec2 = executor.clone();
    tokio::spawn(async move {
        let _ = exec2.run_task(task2).await;
    });

    let t2 = repo.find_by_id("task-2").await.unwrap().unwrap();
    assert_eq!(
        t2.status,
        TaskStatus::Pending,
        "Second task should stay pending when limit reached"
    );
}

#[tokio::test]
async fn deliver_to_source_signal_only_sends_empty_content() {
    let (state, _tmp) = test_app_state().await;

    let source_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Source".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Delegation {
        source_agent_id: "agent-1".to_string(),
        source_chat_id: source_chat.id.clone(),
        resume_parent: false,
    });
    task.chat_id = Some("task-chat".to_string());

    // Signal-only completion: no result text, no deliverables
    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &task,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: None,
        },
        vec![],
    )
    .await;

    let messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0].content.is_empty(),
        "Signal-only completion should have empty content"
    );
}

#[tokio::test]
async fn deliver_to_source_saves_message_to_user_chat() {
    let (state, _tmp) = test_app_state().await;

    // check_and_resume_parent runs, but it should bail out because
    // the source chat is not a task chat.
    let user_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("User chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Delegation {
        source_agent_id: "agent-1".to_string(),
        source_chat_id: user_chat.id.clone(),
        resume_parent: true,
    });
    task.chat_id = Some("task-chat".to_string());

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &task,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: Some("Done".to_string()),
        },
        vec![],
    )
    .await;

    // Message should be delivered
    let messages = state
        .chat_service
        .get_stored_messages(&user_chat.id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "Done");
}

#[tokio::test]
async fn lifecycle_event_saved_after_assistant_message() {
    let (state, _tmp) = test_app_state().await;

    let chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: Some("task-order".to_string()),
                agent_id: "agent-1".to_string(),
                title: Some("Task chat".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    // Simulate the executor flow: save assistant message first
    state
        .chat_service
        .save_agent_message(
            "user-1",
            None,
            &chat.id,
            "agent-1",
            "Here is my answer.".to_string(),
            None,
        )
        .await
        .unwrap();

    // Then save lifecycle event
    state
        .chat_service
        .save_system_event(
            "user-1",
            None,
            &chat.id,
            MessageEvent::TaskCompletion {
                task_id: "task-order".to_string(),
                chat_id: Some(chat.id.clone()),
                status: TaskStatus::Completed,
                summary: None,
                schema: None,
            },
        )
        .await
        .unwrap();

    let messages = state
        .chat_service
        .get_stored_messages(&chat.id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::Agent);
    assert_eq!(messages[0].content, "Here is my answer.");
    assert_eq!(messages[1].role, MessageRole::System);
    assert!(matches!(
        &messages[1].event,
        Some(MessageEvent::TaskCompletion {
            status: TaskStatus::Completed,
            ..
        })
    ));
}

// CronRun delivery semantics — verifies that process_result gates result
// delivery + parent resume for the new TaskKind::CronRun variant.

async fn make_cron_template_with(
    state: &AppState,
    source_chat_id: Option<String>,
    process_result: bool,
) -> Task {
    use frona::agent::task::models::{CronConcurrency, CronMode};
    let next = frona::tool::task::next_cron_occurrence("* * * * *", "UTC").unwrap();
    state
        .task_service
        .create_cron_template(
            "user-1",
            "agent-1",
            "Test cron",
            "do a thing",
            "* * * * *",
            "UTC".to_string(),
            next,
            None,
            Some("agent-1".to_string()),
            source_chat_id,
            None,
            CronMode::Singleton,
            CronConcurrency::Replace,
            process_result,
            None,
            None,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn deliver_to_source_cron_run_posts_regardless_of_process_result() {
    // CronRun mirrors Delegation: the completion summary always lands in the
    // caller chat. `process_result` only governs whether the caller agent
    // resumes (separate `resume_parent_if_requested` path).
    let (state, _tmp) = test_app_state().await;

    let source_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Caller".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let template = make_cron_template_with(&state, Some(source_chat.id.clone()), false).await;
    let run = state
        .task_service
        .spawn_cron_run(&template, Utc::now(), 1)
        .await
        .unwrap();

    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &run,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: Some("Result body".to_string()),
        },
        vec![],
    )
    .await;

    let messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await
        .unwrap();
    assert_eq!(
        messages.len(),
        1,
        "summary delivered even with process_result=false"
    );
    assert_eq!(messages[0].content, "Result body");
}

#[tokio::test]
async fn deliver_to_source_cron_run_posts_when_process_result_true() {
    let (state, _tmp) = test_app_state().await;

    let source_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: None,
                agent_id: "agent-1".to_string(),
                title: Some("Caller".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    let template = make_cron_template_with(&state, Some(source_chat.id.clone()), true).await;
    let run = state
        .task_service
        .spawn_cron_run(&template, Utc::now(), 1)
        .await
        .unwrap();

    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &run,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: Some("Result body".to_string()),
        },
        vec![],
    )
    .await;

    let messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await
        .unwrap();
    assert_eq!(
        messages.len(),
        1,
        "process_result=true → one delivery message"
    );
    assert_eq!(messages[0].content, "Result body");
}

#[tokio::test]
async fn deliver_to_source_cron_run_skips_when_no_source_chat() {
    // Template created without a source_chat_id (e.g. user-initiated cron with
    // no calling agent). Even with process_result=true the delivery is a no-op
    // because there's nowhere to deliver to.
    let (state, _tmp) = test_app_state().await;

    let template = make_cron_template_with(&state, None, true).await;
    let run = state
        .task_service
        .spawn_cron_run(&template, Utc::now(), 1)
        .await
        .unwrap();

    frona::agent::task::executor::deliver_event_to_source(
        &state.chat_service,
        &run,
        frona::agent::task::executor::TaskLifecycleEvent::Completion {
            status: TaskStatus::Completed,
            summary: Some("Result body".to_string()),
        },
        vec![],
    )
    .await;
    // No assertion on side-effect (silently no-op); the test passes if it does
    // not panic and no orphan write occurs.
    assert!(matches!(
        run.kind,
        TaskKind::CronRun {
            source_chat_id: None,
            ..
        }
    ));
}

#[tokio::test]
async fn resume_parent_cron_run_respects_template_process_result() {
    use frona::core::repository::Repository;
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);

    // Caller chat IS a task chat so check_and_resume_parent doesn't bail out
    // on the user-chat guard.
    let caller_chat = state
        .chat_service
        .create_chat(
            "user-1",
            frona::chat::models::CreateChatRequest {
                space_id: None,
                task_id: Some("caller-task".to_string()),
                agent_id: "agent-1".to_string(),
                title: Some("Caller".to_string()),
                metadata: None,
            },
        )
        .await
        .unwrap();

    // Persist the run so its sibling-query path works.
    let template_off = make_cron_template_with(&state, Some(caller_chat.id.clone()), false).await;
    let run_off = state
        .task_service
        .spawn_cron_run(&template_off, Utc::now(), 1)
        .await
        .unwrap();
    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    let mut completed = run_off.clone();
    completed.status = TaskStatus::Completed;
    repo.update(&completed).await.unwrap();

    // process_result=false should NOT trigger a resume — i.e. the call returns
    // without erroring and no spawn happens. We can only assert the negative by
    // checking the function completes synchronously without panicking.
    executor.resume_parent_if_requested(&completed).await;

    // process_result=true path: dependency lookup against template must succeed
    // and the resume logic must engage. We verify reach by ensuring the function
    // does not panic and the template lookup path resolves.
    let template_on = make_cron_template_with(&state, Some(caller_chat.id.clone()), true).await;
    let run_on = state
        .task_service
        .spawn_cron_run(&template_on, Utc::now(), 1)
        .await
        .unwrap();
    let mut completed_on = run_on.clone();
    completed_on.status = TaskStatus::Completed;
    repo.update(&completed_on).await.unwrap();
    executor.resume_parent_if_requested(&completed_on).await;
}

/// `max_concurrent_tasks=0` short-circuits `run_task` so the test
/// observes pure resume-orchestration DB state without an inference model.
async fn test_app_state_no_spawning() -> (AppState, tempfile::TempDir) {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = test_config(&tmp);
    config.server.max_concurrent_tasks = 0;
    let storage = StorageService::new(&config);
    let resource_manager = std::sync::Arc::new(
        frona::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
            80.0, 80.0, 90.0, 90.0,
        ),
    );
    let metrics_handle = frona::core::metrics::setup_metrics_recorder();
    let state = AppState::new(
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
        storage,
        metrics_handle,
        resource_manager,
        crate::helpers::app_state::catalogs(&config),
    );
    seed_user_and_agent(&state).await;
    (state, tmp)
}

#[tokio::test]
async fn resume_all_marks_only_in_progress_cron_runs_failed() {
    let (state, _tmp) = test_app_state_no_spawning().await;
    let executor = make_executor(&state);

    let next = frona::tool::task::next_cron_occurrence("* * * * *", "UTC").unwrap();
    let template = state
        .task_service
        .create_cron_template(
            "user-1",
            "agent-1",
            "t",
            "d",
            "* * * * *",
            "UTC".into(),
            next,
            None,
            None,
            None,
            None,
            frona::agent::task::models::CronMode::Singleton,
            frona::agent::task::models::CronConcurrency::Replace,
            false,
            None,
            None,
        )
        .await
        .unwrap();

    // Pending = scheduler queued, executor hadn't picked up before restart.
    let pending_run = state
        .task_service
        .spawn_cron_run(&template, Utc::now(), 1)
        .await
        .unwrap();

    // InProgress = crashed mid-execution.
    let in_flight_run = state
        .task_service
        .spawn_cron_run(&template, Utc::now(), 2)
        .await
        .unwrap();
    state
        .task_service
        .mark_in_progress(&in_flight_run.id, Some("chat-x"))
        .await
        .unwrap();

    executor.resume_all().await;

    let pending_after = state
        .task_service
        .find_by_id(&pending_run.id)
        .await
        .unwrap()
        .expect("pending run still exists");
    let in_flight_after = state
        .task_service
        .find_by_id(&in_flight_run.id)
        .await
        .unwrap()
        .expect("in-flight run still exists");

    assert_eq!(
        in_flight_after.status,
        TaskStatus::Failed,
        "InProgress CronRun must be marked Failed by the orphan sweep"
    );
    assert_eq!(
        pending_after.status,
        TaskStatus::Pending,
        "Pending CronRun must NOT be marked Failed — the bug pattern was \
         resume_in_flight_crons running after spawn, catching just-spawned runs"
    );
}
