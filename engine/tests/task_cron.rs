use chrono::{Duration, Utc};
use frona::agent::task::models::{Task, TaskKind, TaskStatus};
use frona::agent::task::service::TaskService;
use frona::db::init as db;
use frona::db::repo::generic::SurrealRepo;
use frona::core::repository::Repository;
use frona::tool::schedule::next_cron_occurrence;
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db::setup_schema(&db).await.unwrap();
    db
}

fn make_task_service(db: Surreal<Db>) -> TaskService {
    TaskService::new(SurrealRepo::new(db))
}

#[tokio::test]
async fn create_cron_template_stores_correctly() {
    let db = test_db().await;
    let svc = make_task_service(db);

    let next = next_cron_occurrence("0 9 * * *").unwrap();
    let task = svc
        .create_cron_template(
            "user-1",
            "agent-1",
            "Daily check",
            "Check things every day",
            "0 9 * * *",
            next,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(task.user_id, "user-1");
    assert_eq!(task.agent_id, "agent-1");
    assert_eq!(task.title, "Daily check");
    assert_eq!(task.status, TaskStatus::Pending);

    match &task.kind {
        TaskKind::Cron {
            cron_expression,
            next_run_at,
            source_agent_id,
            source_chat_id,
        } => {
            assert_eq!(cron_expression, "0 9 * * *");
            assert_eq!(*next_run_at, Some(next));
            assert!(source_agent_id.is_none());
            assert!(source_chat_id.is_none());
        }
        _ => panic!("Expected Cron variant"),
    }
}

#[tokio::test]
async fn create_cron_template_with_source_provenance() {
    let db = test_db().await;
    let svc = make_task_service(db);

    let next = next_cron_occurrence("*/5 * * * *").unwrap();
    let task = svc
        .create_cron_template(
            "user-1",
            "agent-researcher",
            "Frequent poll",
            "Poll data source",
            "*/5 * * * *",
            next,
            Some("agent-system".into()),
            Some("chat-origin".into()),
            None,
        )
        .await
        .unwrap();

    match &task.kind {
        TaskKind::Cron {
            source_agent_id,
            source_chat_id,
            ..
        } => {
            assert_eq!(source_agent_id.as_deref(), Some("agent-system"));
            assert_eq!(source_chat_id.as_deref(), Some("chat-origin"));
        }
        _ => panic!("Expected Cron variant"),
    }
}

#[tokio::test]
async fn advance_cron_template_updates_next_run_at() {
    let db = test_db().await;
    let svc = make_task_service(db);

    let first_next = next_cron_occurrence("0 9 * * *").unwrap();
    let template = svc
        .create_cron_template(
            "user-1",
            "agent-1",
            "Daily task",
            "description",
            "0 9 * * *",
            first_next,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let new_next = first_next + Duration::days(1);
    let advanced = svc
        .advance_cron_template(&template.id, new_next, Some("chat-1"))
        .await
        .unwrap();

    match &advanced.kind {
        TaskKind::Cron { next_run_at, .. } => {
            assert_eq!(*next_run_at, Some(new_next));
        }
        _ => panic!("Expected Cron variant"),
    }
    assert_eq!(advanced.chat_id.as_deref(), Some("chat-1"));
}

#[tokio::test]
async fn find_due_cron_templates_returns_only_due_pending() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db.clone());

    let past = Utc::now() - Duration::minutes(5);
    let t1 = svc
        .create_cron_template("user-1", "agent-1", "Due task", "desc", "0 9 * * *", past, None, None, None)
        .await
        .unwrap();

    let future = Utc::now() + Duration::hours(2);
    let _t2 = svc
        .create_cron_template("user-1", "agent-1", "Future task", "desc", "0 11 * * *", future, None, None, None)
        .await
        .unwrap();

    let mut cancelled = svc
        .create_cron_template("user-1", "agent-1", "Cancelled task", "desc", "0 8 * * *", past, None, None, None)
        .await
        .unwrap();
    cancelled.status = TaskStatus::Cancelled;
    repo.update(&cancelled).await.unwrap();

    let due = svc.find_due_cron_templates().await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, t1.id);
}

#[tokio::test]
async fn find_resumable_excludes_cron_templates() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db.clone());

    let now = Utc::now();
    let direct_task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Direct task".to_string(),
        description: "A regular task".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&direct_task).await.unwrap();

    let past = Utc::now() - Duration::minutes(5);
    svc.create_cron_template("user-1", "agent-1", "Cron template", "desc", "0 9 * * *", past, None, None, None)
        .await
        .unwrap();

    let resumable = svc.find_resumable().await.unwrap();
    assert_eq!(resumable.len(), 1);
    assert_eq!(resumable[0].id, direct_task.id);
    assert!(matches!(resumable[0].kind, TaskKind::Direct));
}

#[tokio::test]
async fn find_resumable_includes_in_progress_tasks() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: Some("chat-1".to_string()),
        title: "Was running when server crashed".to_string(),
        description: "Interrupted task".to_string(),
        status: TaskStatus::InProgress,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&task).await.unwrap();

    let resumable = svc.find_resumable().await.unwrap();
    assert_eq!(resumable.len(), 1);
    assert_eq!(resumable[0].id, task.id);
    assert_eq!(resumable[0].status, TaskStatus::InProgress);
}

#[tokio::test]
async fn find_resumable_includes_delegation_tasks() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-2".to_string(),
        space_id: None,
        chat_id: None,
        title: "Delegated work".to_string(),
        description: "Work delegated from another agent".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Delegation {
            source_agent_id: "agent-1".to_string(),
            source_chat_id: "chat-origin".to_string(),
            deliver_directly: false,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&task).await.unwrap();

    let resumable = svc.find_resumable().await.unwrap();
    assert_eq!(resumable.len(), 1);
    assert_eq!(resumable[0].id, task.id);
    assert!(matches!(resumable[0].kind, TaskKind::Delegation { .. }));
}

#[tokio::test]
async fn find_resumable_excludes_terminal_states() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();
    for (i, status) in [TaskStatus::Completed, TaskStatus::Failed, TaskStatus::Cancelled]
        .iter()
        .enumerate()
    {
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: "user-1".to_string(),
            agent_id: "agent-1".to_string(),
            space_id: None,
            chat_id: None,
            title: format!("Terminal task {}", i),
            description: "Should not resume".to_string(),
            status: status.clone(),
            kind: TaskKind::Direct,
            run_at: None,
            result_summary: None,
            error_message: None,
            created_at: now + Duration::seconds(i as i64),
            updated_at: now,
        };
        repo.create(&task).await.unwrap();
    }

    let resumable = svc.find_resumable().await.unwrap();
    assert!(resumable.is_empty(), "Terminal tasks should not be resumable");
}

#[tokio::test]
async fn find_resumable_orders_by_created_at_asc() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let base = Utc::now();
    let mut ids = Vec::new();
    for i in 0..3 {
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: "user-1".to_string(),
            agent_id: "agent-1".to_string(),
            space_id: None,
            chat_id: None,
            title: format!("Task {}", i),
            description: "desc".to_string(),
            status: TaskStatus::Pending,
            kind: TaskKind::Direct,
            run_at: None,
            result_summary: None,
            error_message: None,
            created_at: base + Duration::seconds(i),
            updated_at: base,
        };
        ids.push(task.id.clone());
        repo.create(&task).await.unwrap();
    }

    let resumable = svc.find_resumable().await.unwrap();
    assert_eq!(resumable.len(), 3);
    assert_eq!(resumable[0].id, ids[0], "Oldest task first");
    assert_eq!(resumable[1].id, ids[1]);
    assert_eq!(resumable[2].id, ids[2], "Newest task last");
}

#[tokio::test]
async fn find_resumable_mixed_scenario() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();

    // Pending Direct — should resume
    let pending_direct = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Pending direct".to_string(),
        description: "d".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&pending_direct).await.unwrap();

    // InProgress Delegation — should resume
    let in_progress_delegation = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-2".to_string(),
        space_id: None,
        chat_id: Some("chat-1".to_string()),
        title: "In-progress delegation".to_string(),
        description: "d".to_string(),
        status: TaskStatus::InProgress,
        kind: TaskKind::Delegation {
            source_agent_id: "agent-1".to_string(),
            source_chat_id: "chat-origin".to_string(),
            deliver_directly: false,
        },
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now + Duration::seconds(1),
        updated_at: now,
    };
    repo.create(&in_progress_delegation).await.unwrap();

    // Cron template — should NOT resume
    let past = Utc::now() - Duration::minutes(5);
    svc.create_cron_template("user-1", "agent-1", "Cron tmpl", "d", "0 9 * * *", past, None, None, None)
        .await
        .unwrap();

    // Completed Direct — should NOT resume
    let completed = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Completed".to_string(),
        description: "d".to_string(),
        status: TaskStatus::Completed,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: Some("done".to_string()),
        error_message: None,
        created_at: now + Duration::seconds(3),
        updated_at: now,
    };
    repo.create(&completed).await.unwrap();

    // Failed Direct — should NOT resume
    let failed = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Failed".to_string(),
        description: "d".to_string(),
        status: TaskStatus::Failed,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: Some("err".to_string()),
        created_at: now + Duration::seconds(4),
        updated_at: now,
    };
    repo.create(&failed).await.unwrap();

    let resumable = svc.find_resumable().await.unwrap();
    assert_eq!(resumable.len(), 2, "Only Pending/InProgress non-Cron tasks");

    let resumable_ids: Vec<&str> = resumable.iter().map(|t| t.id.as_str()).collect();
    assert!(resumable_ids.contains(&pending_direct.id.as_str()));
    assert!(resumable_ids.contains(&in_progress_delegation.id.as_str()));

    // Verify ordering: created_at ASC
    assert_eq!(resumable[0].id, pending_direct.id);
    assert_eq!(resumable[1].id, in_progress_delegation.id);
}

#[tokio::test]
async fn find_due_cron_templates_unaffected_by_restart() {
    let db = test_db().await;
    let svc = make_task_service(db);

    let past = Utc::now() - Duration::minutes(10);
    let template = svc
        .create_cron_template("user-1", "agent-1", "Hourly", "desc", "0 * * * *", past, None, None, None)
        .await
        .unwrap();

    // Simulate what happens on restart: cron fires, template advanced
    let next = next_cron_occurrence("0 * * * *").unwrap();
    svc.advance_cron_template(&template.id, next, Some("chat-1"))
        .await
        .unwrap();

    // After advancing, template should no longer be due
    let due = svc.find_due_cron_templates().await.unwrap();
    assert!(due.is_empty(), "Fired template should not be due again");
}

#[tokio::test]
async fn mark_in_progress_then_find_resumable() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Task to resume".to_string(),
        description: "desc".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&task).await.unwrap();

    // Simulate executor picking up the task
    let in_progress = svc.mark_in_progress(&task.id, Some("chat-new")).await.unwrap();
    assert_eq!(in_progress.status, TaskStatus::InProgress);
    assert_eq!(in_progress.chat_id.as_deref(), Some("chat-new"));

    // Simulate server crash & restart: InProgress task should still be resumable
    let resumable = svc.find_resumable().await.unwrap();
    assert_eq!(resumable.len(), 1);
    assert_eq!(resumable[0].id, task.id);
    assert_eq!(resumable[0].status, TaskStatus::InProgress);
    assert_eq!(resumable[0].chat_id.as_deref(), Some("chat-new"));
}

#[tokio::test]
async fn completed_during_execution_not_resumable() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Will complete".to_string(),
        description: "desc".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&task).await.unwrap();

    // Simulate full lifecycle: pending → in_progress → completed
    svc.mark_in_progress(&task.id, Some("chat-1")).await.unwrap();
    svc.mark_completed(&task.id, Some("Done".to_string())).await.unwrap();

    // After completion, task should NOT be resumable
    let resumable = svc.find_resumable().await.unwrap();
    assert!(resumable.is_empty());

    let task = svc.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.result_summary.as_deref(), Some("Done"));
}

#[tokio::test]
async fn failed_during_execution_not_resumable() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Will fail".to_string(),
        description: "desc".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&task).await.unwrap();

    svc.mark_in_progress(&task.id, None).await.unwrap();
    svc.mark_failed(&task.id, "LLM error".to_string()).await.unwrap();

    let resumable = svc.find_resumable().await.unwrap();
    assert!(resumable.is_empty());

    let task = svc.find_by_id(&task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.error_message.as_deref(), Some("LLM error"));
}

#[tokio::test]
async fn cancelled_during_execution_not_resumable() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Will cancel".to_string(),
        description: "desc".to_string(),
        status: TaskStatus::InProgress,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&task).await.unwrap();

    svc.mark_cancelled(&task.id).await.unwrap();

    let resumable = svc.find_resumable().await.unwrap();
    assert!(resumable.is_empty());
}

#[tokio::test]
async fn cron_template_lifecycle_simulation() {
    let db = test_db().await;
    let svc = make_task_service(db);

    let first_run = Utc::now() - Duration::minutes(1);
    let template = svc
        .create_cron_template(
            "user-1",
            "agent-1",
            "Hourly check",
            "Check everything",
            "0 * * * *",
            first_run,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let due = svc.find_due_cron_templates().await.unwrap();
    assert_eq!(due.len(), 1);

    // Execute the cron directly (no CronRun child) and advance
    let second_run = next_cron_occurrence("0 * * * *").unwrap();
    let advanced = svc
        .advance_cron_template(&template.id, second_run, Some("chat-1"))
        .await
        .unwrap();

    match &advanced.kind {
        TaskKind::Cron { next_run_at, .. } => {
            assert_eq!(*next_run_at, Some(second_run));
            assert!(second_run > Utc::now());
        }
        _ => panic!("Expected Cron"),
    }

    let due_after = svc.find_due_cron_templates().await.unwrap();
    assert!(due_after.is_empty(), "Template should no longer be due after advancing");

    let template_check = svc.find_by_id(&template.id).await.unwrap().unwrap();
    assert_eq!(template_check.status, TaskStatus::Pending, "Template stays Pending");
}

#[tokio::test]
async fn deferred_task_found_when_due() {
    let db = test_db().await;
    let repo: SurrealRepo<Task> = SurrealRepo::new(db.clone());
    let svc = make_task_service(db);

    let now = Utc::now();

    // Deferred task due in the past
    let deferred_due = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Past deferred".to_string(),
        description: "Should be found".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: Some(now - Duration::minutes(5)),
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&deferred_due).await.unwrap();

    // Deferred task due in the future
    let deferred_future = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Future deferred".to_string(),
        description: "Should not be found".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: Some(now + Duration::hours(2)),
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&deferred_future).await.unwrap();

    // Immediate task (no run_at)
    let immediate = Task {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: "user-1".to_string(),
        agent_id: "agent-1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Immediate".to_string(),
        description: "No run_at".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: None,
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };
    repo.create(&immediate).await.unwrap();

    let deferred = svc.find_deferred_due().await.unwrap();
    assert_eq!(deferred.len(), 1, "Only past-due deferred tasks");
    assert_eq!(deferred[0].id, deferred_due.id);
}

#[tokio::test]
async fn deferred_task_excludes_cron() {
    let db = test_db().await;
    let svc = make_task_service(db);

    let past = Utc::now() - Duration::minutes(5);

    // Cron template with past next_run_at — should NOT appear in deferred results
    svc.create_cron_template("user-1", "agent-1", "Cron", "desc", "0 9 * * *", past, None, None, None)
        .await
        .unwrap();

    let deferred = svc.find_deferred_due().await.unwrap();
    assert!(deferred.is_empty(), "Cron tasks excluded from deferred query");
}

#[tokio::test]
async fn task_run_at_serialization() {
    let now = Utc::now();
    let task = Task {
        id: "t1".to_string(),
        user_id: "u1".to_string(),
        agent_id: "a1".to_string(),
        space_id: None,
        chat_id: None,
        title: "Deferred".to_string(),
        description: "desc".to_string(),
        status: TaskStatus::Pending,
        kind: TaskKind::Direct,
        run_at: Some(now),
        result_summary: None,
        error_message: None,
        created_at: now,
        updated_at: now,
    };

    let json = serde_json::to_string(&task).unwrap();
    let deserialized: Task = serde_json::from_str(&json).unwrap();
    assert!(deserialized.run_at.is_some());

    let task_no_run_at = Task {
        run_at: None,
        ..task
    };
    let json = serde_json::to_string(&task_no_run_at).unwrap();
    let deserialized: Task = serde_json::from_str(&json).unwrap();
    assert!(deserialized.run_at.is_none());
}
