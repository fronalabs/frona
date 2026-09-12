//! E2E tests driving `scheduler::execute_cron` against a fully wired AppState.

mod helpers;

use std::sync::Arc;

use frona::agent::task::executor::TaskExecutor;
use frona::agent::task::models::{CronConcurrency, CronMode, Task, TaskKind, TaskStatus};
use frona::core::config::Config;
use frona::core::state::AppState;
use frona::db::init as db;
use frona::scheduler::execute_cron;
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
    (state, tmp)
}

fn install_executor(state: &AppState) -> Arc<TaskExecutor> {
    state.task_executor.clone()
}

async fn make_template(state: &AppState, mode: CronMode, concurrency: CronConcurrency) -> Task {
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
            None,
            None,
            None,
            mode,
            concurrency,
            false,
            None,
            None,
        )
        .await
        .unwrap()
}

/// Bypass `execute_cron` to avoid the spawn race against the no-model-registry
/// failure path. Returns the run + a token registered on the executor.
async fn arrange_in_flight_run(
    state: &AppState,
    executor: &Arc<TaskExecutor>,
    template: &Task,
    sequence_num: u64,
) -> (Task, tokio_util::sync::CancellationToken) {
    let run = state
        .task_service
        .spawn_cron_run(template, chrono::Utc::now(), sequence_num)
        .await
        .unwrap();
    state
        .task_service
        .mark_in_progress(&run.id, Some("chat-mock"))
        .await
        .unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    executor
        .register_cancellation(&run.agent_id, &run.id, token.clone())
        .await;
    (run, token)
}

#[tokio::test]
async fn execute_cron_spawns_cron_run_with_correct_linkage() {
    let (state, _tmp) = test_app_state().await;
    install_executor(&state);
    let template = make_template(&state, CronMode::Singleton, CronConcurrency::Replace).await;

    execute_cron(&state, &template).await.unwrap();

    let runs = state
        .task_service
        .find_runs_by_cron(&template.id)
        .await
        .unwrap();
    assert_eq!(
        runs.len(),
        1,
        "first fire should produce exactly one CronRun"
    );

    match &runs[0].kind {
        TaskKind::CronRun {
            source_cron_id,
            sequence_num,
            ..
        } => {
            assert_eq!(source_cron_id, &template.id);
            assert_eq!(*sequence_num, 1);
        }
        _ => panic!("Expected CronRun, got {:?}", runs[0].kind),
    }
}

#[tokio::test]
async fn execute_cron_increments_sequence_num_across_fires() {
    let (state, _tmp) = test_app_state().await;
    install_executor(&state);
    let template = make_template(&state, CronMode::PerInstance, CronConcurrency::Allow).await;

    for _ in 0..3 {
        execute_cron(&state, &template).await.unwrap();
    }

    let runs = state
        .task_service
        .find_runs_by_cron(&template.id)
        .await
        .unwrap();
    assert_eq!(runs.len(), 3);

    let mut seqs: Vec<u64> = runs
        .iter()
        .map(|r| match &r.kind {
            TaskKind::CronRun { sequence_num, .. } => *sequence_num,
            _ => panic!("expected CronRun"),
        })
        .collect();
    seqs.sort();
    assert_eq!(seqs, vec![1, 2, 3]);
}

#[tokio::test]
async fn execute_cron_forbid_skips_while_previous_in_flight() {
    let (state, _tmp) = test_app_state().await;
    let executor = install_executor(&state);
    let template = make_template(&state, CronMode::PerInstance, CronConcurrency::Forbid).await;

    let (_run1, _token) = arrange_in_flight_run(&state, &executor, &template, 1).await;

    execute_cron(&state, &template).await.unwrap();
    let runs = state
        .task_service
        .find_runs_by_cron(&template.id)
        .await
        .unwrap();
    assert_eq!(
        runs.len(),
        1,
        "Forbid policy must skip while previous run is in flight"
    );
}

#[tokio::test]
async fn execute_cron_replace_cancels_in_flight_and_spawns_new() {
    let (state, _tmp) = test_app_state().await;
    let executor = install_executor(&state);
    let template = make_template(&state, CronMode::Singleton, CronConcurrency::Replace).await;

    let (_run1, cancel_token) = arrange_in_flight_run(&state, &executor, &template, 1).await;
    assert!(!cancel_token.is_cancelled());

    execute_cron(&state, &template).await.unwrap();

    let runs = state
        .task_service
        .find_runs_by_cron(&template.id)
        .await
        .unwrap();
    assert_eq!(
        runs.len(),
        2,
        "Replace policy must spawn a new CronRun alongside the cancelled one"
    );
    assert!(
        cancel_token.is_cancelled(),
        "Replace policy must fire the in-flight run's cancellation token"
    );
}

#[tokio::test]
async fn execute_cron_allow_runs_concurrently() {
    let (state, _tmp) = test_app_state().await;
    let executor = install_executor(&state);
    let template = make_template(&state, CronMode::PerInstance, CronConcurrency::Allow).await;

    let (run1, cancel_token) = arrange_in_flight_run(&state, &executor, &template, 1).await;

    execute_cron(&state, &template).await.unwrap();

    let runs = state
        .task_service
        .find_runs_by_cron(&template.id)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2, "Allow policy must spawn concurrent CronRuns");
    assert!(
        !cancel_token.is_cancelled(),
        "Allow policy must leave in-flight runs alone"
    );
    let still = state
        .task_service
        .find_by_id(&run1.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.status, TaskStatus::InProgress);
}

#[tokio::test]
async fn executor_cancel_task_cascades_to_active_cron_runs() {
    let (state, _tmp) = test_app_state().await;
    let executor = install_executor(&state);
    let template = make_template(&state, CronMode::PerInstance, CronConcurrency::Allow).await;

    let (run_a, token_a) = arrange_in_flight_run(&state, &executor, &template, 1).await;
    let (run_b, token_b) = arrange_in_flight_run(&state, &executor, &template, 2).await;
    assert!(!token_a.is_cancelled());
    assert!(!token_b.is_cancelled());

    executor.cancel_task(&template.id).await;

    assert!(
        token_a.is_cancelled(),
        "run_a's token must fire via cascade"
    );
    assert!(
        token_b.is_cancelled(),
        "run_b's token must fire via cascade"
    );
    assert!(
        state
            .task_service
            .find_by_id(&run_a.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        state
            .task_service
            .find_by_id(&run_b.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn executor_cancel_task_on_run_fires_only_that_runs_token() {
    let (state, _tmp) = test_app_state().await;
    let executor = install_executor(&state);
    let template = make_template(&state, CronMode::PerInstance, CronConcurrency::Allow).await;

    let (run_a, token_a) = arrange_in_flight_run(&state, &executor, &template, 1).await;
    let (_run_b, token_b) = arrange_in_flight_run(&state, &executor, &template, 2).await;

    executor.cancel_task(&run_a.id).await;

    assert!(token_a.is_cancelled());
    assert!(!token_b.is_cancelled(), "sibling run must keep running");
}

/// Regression: pre-CronRun, execute_cron stamped task.chat_id and reused it,
/// causing the LLM to gaslight itself on each fire.
#[tokio::test]
async fn execute_cron_does_not_stamp_chat_id_on_template() {
    let (state, _tmp) = test_app_state().await;
    install_executor(&state);
    let template = make_template(&state, CronMode::Singleton, CronConcurrency::Replace).await;
    assert!(template.chat_id.is_none());

    for _ in 0..3 {
        execute_cron(&state, &template).await.unwrap();
    }

    let reloaded = state
        .task_service
        .find_by_id(&template.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        reloaded.chat_id.is_none(),
        "Cron template must never have its chat_id stamped (got {:?})",
        reloaded.chat_id
    );
}
