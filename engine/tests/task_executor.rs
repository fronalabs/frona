use std::sync::Arc;

use chrono::Utc;
use frona::agent::task::executor::TaskExecutor;
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::agent::workspace::AgentWorkspaceManager;
use frona::core::config::Config;
use frona::api::db;
use frona::api::repo::generic::SurrealRepo;
use frona::core::state::AppState;
use frona::chat::broadcast::BroadcastEvent;
use frona::core::repository::Repository;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn test_config(tmp: &tempfile::TempDir) -> Config {
    let base = tmp.path().to_string_lossy().to_string();
    Config {
        port: 0,
        jwt_secret: "test-secret".to_string(),
        surreal_path: format!("{base}/db"),
        static_dir: format!("{base}/static"),
        models_config_path: format!("{base}/models.json"),
        browserless_ws_url: "ws://localhost:0".to_string(),
        browser_profiles_path: format!("{base}/profiles"),
        workspaces_base_path: format!("{base}/workspaces"),
        files_base_path: format!("{base}/files"),
        tools_config_path: format!("{base}/tools.json"),
        shared_config_dir: format!("{base}/config"),
        max_concurrent_tasks: 10,
        scheduler_space_compaction_secs: 3600,
        scheduler_insight_compaction_secs: 7200,
        scheduler_poll_secs: 60,
    }
}

async fn test_app_state() -> (AppState, tempfile::TempDir) {
    let db = test_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let workspaces = AgentWorkspaceManager::new(tmp.path().join("workspaces"));
    let state = AppState::new(db, &config, workspaces);
    (state, tmp)
}

fn make_executor(state: &AppState) -> Arc<TaskExecutor> {
    Arc::new(TaskExecutor::new(state.clone()))
}

fn make_task(kind: TaskKind) -> Task {
    let now = Utc::now();
    Task {
        id: uuid::Uuid::new_v4().to_string(),
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
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn ensure_task_chat_creates_when_missing() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct);
    assert!(task.chat_id.is_none());

    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    assert!(!chat_id.is_empty());
    assert_eq!(task.chat_id.as_deref(), Some(chat_id.as_str()));
}

#[tokio::test]
async fn ensure_task_chat_returns_existing() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct);
    task.chat_id = Some("existing-chat-id".to_string());

    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    assert_eq!(chat_id, "existing-chat-id");
    assert_eq!(task.chat_id.as_deref(), Some("existing-chat-id"));
}

#[tokio::test]
async fn save_initial_message_saves_on_first_run() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct);
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    executor
        .save_initial_message_if_needed(&task, &chat_id)
        .await
        .unwrap();

    let messages = state.chat_service.get_stored_messages(&chat_id).await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "Do something");
}

#[tokio::test]
async fn save_initial_message_skips_on_resume() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct);
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    executor
        .save_initial_message_if_needed(&task, &chat_id)
        .await
        .unwrap();
    executor
        .save_initial_message_if_needed(&task, &chat_id)
        .await
        .unwrap();

    let messages = state.chat_service.get_stored_messages(&chat_id).await;
    assert_eq!(messages.len(), 1);
}

#[tokio::test]
async fn handle_cancelled_saves_partial_text() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct);
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    executor
        .handle_cancelled(&task, &chat_id, "partial output".to_string())
        .await
        .unwrap();

    let messages = state.chat_service.get_stored_messages(&chat_id).await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "partial output");

    let updated = repo.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(updated.status, TaskStatus::Cancelled);
}

#[tokio::test]
async fn handle_cancelled_skips_empty_text() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct);
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    executor
        .handle_cancelled(&task, &chat_id, String::new())
        .await
        .unwrap();

    let messages = state.chat_service.get_stored_messages(&chat_id).await;
    assert!(messages.is_empty());

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
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Delegation {
        source_agent_id: "agent-1".to_string(),
        source_chat_id: source_chat.id.clone(),
        deliver_directly: true,
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
        .await;
    assert_eq!(source_messages.len(), 1);
    assert!(source_messages[0].content.contains("something broke"));
}

#[tokio::test]
async fn handle_completed_marks_done() {
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
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Delegation {
        source_agent_id: "agent-1".to_string(),
        source_chat_id: source_chat.id.clone(),
        deliver_directly: true,
    });
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    executor
        .handle_completed(
            &task,
            &chat_id,
            "Full text".to_string(),
            "Summary segment".to_string(),
            vec![],
        )
        .await
        .unwrap();

    let updated = repo.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(updated.status, TaskStatus::Completed);
    assert_eq!(updated.result_summary.as_deref(), Some("Summary segment"));

    let source_messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await;
    assert_eq!(source_messages.len(), 1);
}

#[tokio::test]
async fn handle_completed_waits_for_children() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut task = make_task(TaskKind::Direct);
    let chat_id = executor.ensure_task_chat(&mut task).await.unwrap();

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());
    repo.create(&task).await.unwrap();

    let child = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-2".to_string(),
        space_id: None,
        chat_id: None,
        title: "Child task".to_string(),
        description: "Child work".to_string(),
        status: TaskStatus::InProgress,
        kind: TaskKind::Delegation {
            source_agent_id: "agent-1".to_string(),
            source_chat_id: chat_id.clone(),
            deliver_directly: false,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    repo.create(&child).await.unwrap();

    executor
        .handle_completed(&task, &chat_id, "Done".to_string(), String::new(), vec![])
        .await
        .unwrap();

    let parent = repo.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(parent.status, TaskStatus::Pending, "Parent should stay unchanged when children are incomplete");
}

#[tokio::test]
async fn deliver_to_source_skips_direct_tasks() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let task = make_task(TaskKind::Direct);

    executor
        .deliver_to_source(&task, TaskStatus::Completed, "result".to_string())
        .await;
}

#[tokio::test]
async fn deliver_to_source_sends_to_delegation() {
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
                title: Some("Source".to_string()),
            },
        )
        .await
        .unwrap();

    let mut task = make_task(TaskKind::Delegation {
        source_agent_id: "agent-1".to_string(),
        source_chat_id: source_chat.id.clone(),
        deliver_directly: true,
    });
    task.chat_id = Some("task-chat".to_string());

    executor
        .deliver_to_source(&task, TaskStatus::Completed, "All done".to_string())
        .await;

    let messages = state
        .chat_service
        .get_stored_messages(&source_chat.id)
        .await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "All done");
}

#[tokio::test]
async fn broadcast_task_status_emits_event() {
    let (state, _tmp) = test_app_state().await;
    let executor = make_executor(&state);
    let mut rx = state.broadcast_service.subscribe();

    let mut task = make_task(TaskKind::Direct);
    task.chat_id = Some("chat-123".to_string());

    executor.broadcast_task_status(&task, "completed", Some("All done"));

    let event = rx.recv().await.unwrap();
    match event {
        BroadcastEvent::TaskUpdate {
            user_id,
            task_id,
            status,
            title,
            chat_id,
            source_chat_id,
            result_summary,
        } => {
            assert_eq!(user_id, "user-1");
            assert_eq!(task_id, task.id);
            assert_eq!(status, "completed");
            assert_eq!(title, "Test task");
            assert_eq!(chat_id.as_deref(), Some("chat-123"));
            assert!(source_chat_id.is_none());
            assert_eq!(result_summary.as_deref(), Some("All done"));
        }
        _ => panic!("Expected TaskUpdate event"),
    }
}

#[tokio::test]
async fn concurrency_global_limit() {
    let (mut state, _tmp) = test_app_state().await;
    state.max_concurrent_tasks = 1;
    let executor = make_executor(&state);

    let repo: SurrealRepo<Task> = SurrealRepo::new(state.db.clone());

    let mut task1 = make_task(TaskKind::Direct);
    task1.id = "task-1".to_string();
    task1.status = TaskStatus::InProgress;
    repo.create(&task1).await.unwrap();
    executor.spawn_execution(task1).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut task2 = make_task(TaskKind::Direct);
    task2.id = "task-2".to_string();
    repo.create(&task2).await.unwrap();
    executor.spawn_execution(task2).await.unwrap();

    let t2 = repo.find_by_id("task-2").await.unwrap().unwrap();
    assert_eq!(t2.status, TaskStatus::Pending, "Second task should stay pending when limit reached");
}
