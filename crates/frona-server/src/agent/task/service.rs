use chrono::{DateTime, Utc};

use crate::chat::broadcast::BroadcastService;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::db::repo::tasks::SurrealTaskRepo;

use super::models::{CreateTaskRequest, TaskResponse, UpdateTaskRequest};
use super::models::{Task, TaskKind, TaskStatus};
use super::repository::TaskRepository;

#[derive(Clone)]
pub struct TaskService {
    repo: SurrealTaskRepo,
    broadcast: BroadcastService,
}

impl TaskService {
    pub fn new(repo: SurrealTaskRepo, broadcast: BroadcastService) -> Self {
        Self { repo, broadcast }
    }

    pub fn repo(&self) -> &SurrealTaskRepo {
        &self.repo
    }
}

fn status_str(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "inprogress",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

impl TaskService {
    fn broadcast(&self, task: &Task, status: &str, summary: Option<&str>) {
        self.broadcast.broadcast_task_update(
            &task.user_id,
            &task.id,
            status,
            &task.title,
            task.chat_id.as_deref(),
            task.kind.source_chat_id(),
            summary,
        );
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateTaskRequest,
    ) -> Result<TaskResponse, AppError> {
        let now = chrono::Utc::now();

        let kind = match (req.source_agent_id, req.source_chat_id) {
            (Some(source_agent_id), Some(source_chat_id)) => TaskKind::Delegation {
                source_agent_id,
                source_chat_id,
                resume_parent: req.resume_parent.unwrap_or(false),
            },
            (None, source_chat_id) => TaskKind::Direct { source_chat_id },
            (Some(_), None) => TaskKind::Direct {
                source_chat_id: None,
            },
        };

        if let Some(ref schema) = req.result_schema {
            super::schema::validate_schema_doc(schema).map_err(AppError::Validation)?;
        }

        let task = Task {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            agent_id: req.agent_id,
            space_id: req.space_id,
            chat_id: req.chat_id,
            title: req.title,
            description: req.description.unwrap_or_default(),
            status: TaskStatus::Pending,
            kind,
            run_at: req.run_at,
            result_summary: None,
            error_message: None,
            quarantined: req.quarantined,
            result_schema: req.result_schema,
            result_description: req.result_description,
            created_at: now,
            updated_at: now,
        };

        let task = self.repo.create(&task).await?;
        self.broadcast(&task, "pending", None);
        Ok(task.into())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_signal(
        &self,
        user_id: &str,
        agent_id: String,
        source_chat_id: String,
        title: String,
        description: String,
        resume_parent: bool,
        mode: super::models::SignalMode,
        expected_categories: Vec<String>,
        expected_channels: Vec<String>,
        expected_contacts: Vec<String>,
        expires_at: Option<DateTime<Utc>>,
        max_evaluations: u32,
        result_schema: Option<serde_json::Value>,
    ) -> Result<Task, AppError> {
        if let Some(ref schema) = result_schema {
            super::schema::validate_schema_doc(schema).map_err(AppError::Validation)?;
        }
        let now = chrono::Utc::now();
        let task = Task {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            agent_id,
            space_id: None,
            chat_id: None,
            title,
            description,
            status: TaskStatus::Pending,
            kind: TaskKind::Signal {
                source_chat_id,
                resume_parent,
                mode,
                expected_categories,
                expected_channels,
                expected_contacts,
                expires_at,
                max_evaluations,
                evaluation_count: 0,
            },
            run_at: None,
            result_summary: None,
            error_message: None,
            quarantined: true,
            result_schema,
            result_description: None,
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&task).await
    }

    pub async fn list_pending_signal_tasks(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_pending_signal_tasks().await
    }

    pub async fn find_expired_signal_tasks(&self) -> Result<Vec<Task>, AppError> {
        self.repo
            .find_expired_signal_tasks(chrono::Utc::now())
            .await
    }

    pub async fn save(&self, task: &Task) -> Result<Task, AppError> {
        self.repo.update(task).await
    }

    pub async fn list_active(&self, user_id: &str) -> Result<Vec<TaskResponse>, AppError> {
        let tasks = self.repo.find_active_by_user_id(user_id).await?;
        Ok(tasks.into_iter().map(Into::into).collect())
    }

    pub async fn list_all(&self, user_id: &str) -> Result<Vec<TaskResponse>, AppError> {
        let tasks = self.repo.find_all_by_user_id(user_id).await?;
        Ok(tasks.into_iter().map(Into::into).collect())
    }

    pub async fn update(
        &self,
        user_id: &str,
        task_id: &str,
        req: UpdateTaskRequest,
    ) -> Result<TaskResponse, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        if task.user_id != user_id {
            return Err(AppError::Forbidden("Not your task".into()));
        }

        if let Some(title) = req.title {
            task.title = title;
        }
        if let Some(description) = req.description {
            task.description = description;
        }
        if let Some(status) = req.status {
            task.status = status;
        }
        task.updated_at = chrono::Utc::now();

        let task = self.repo.update(&task).await?;
        self.broadcast(
            &task,
            status_str(&task.status),
            task.result_summary.as_deref(),
        );
        Ok(task.into())
    }

    /// Per-run chats/messages cascade via the `cascade_delete_task_chat` DB event.
    /// Runtime token cancellation is the caller's job via `TaskExecutor::cancel_task`.
    pub async fn delete(&self, user_id: &str, task_id: &str) -> Result<(), AppError> {
        let task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        if task.user_id != user_id {
            return Err(AppError::Forbidden("Not your task".into()));
        }

        if matches!(task.kind, TaskKind::Cron { .. }) {
            let runs = self.find_runs_by_cron(task_id).await.unwrap_or_default();
            for run in runs {
                let _ = self.repo.delete(&run.id).await;
            }
        }

        self.repo.delete(task_id).await?;
        self.broadcast(&task, "cancelled", None);
        Ok(())
    }

    pub async fn find_resumable(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_resumable(chrono::Utc::now()).await
    }

    pub async fn find_by_id(&self, task_id: &str) -> Result<Option<Task>, AppError> {
        self.repo.find_by_id(task_id).await
    }

    pub async fn find_by_chat_id(&self, chat_id: &str) -> Result<Option<Task>, AppError> {
        self.repo.find_by_chat_id(chat_id).await
    }

    pub async fn find_by_source_chat_id(
        &self,
        source_chat_id: &str,
    ) -> Result<Vec<Task>, AppError> {
        self.repo.find_by_source_chat_id(source_chat_id).await
    }

    pub async fn mark_in_progress(
        &self,
        task_id: &str,
        chat_id: Option<&str>,
    ) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        task.status = TaskStatus::InProgress;
        if let Some(cid) = chat_id {
            task.chat_id = Some(cid.to_string());
        }
        task.updated_at = chrono::Utc::now();

        let task = self.repo.update(&task).await?;
        self.broadcast(&task, "inprogress", None);
        Ok(task)
    }

    pub async fn mark_completed(
        &self,
        task_id: &str,
        summary: Option<String>,
    ) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        task.status = TaskStatus::Completed;
        task.result_summary = summary;
        task.updated_at = chrono::Utc::now();

        let task = self.repo.update(&task).await?;
        self.broadcast(&task, "completed", task.result_summary.as_deref());
        Ok(task)
    }

    pub async fn mark_failed(&self, task_id: &str, error: String) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        task.status = TaskStatus::Failed;
        task.error_message = Some(error);
        task.updated_at = chrono::Utc::now();

        let task = self.repo.update(&task).await?;
        self.broadcast(&task, "failed", task.error_message.as_deref());
        Ok(task)
    }

    pub async fn mark_deferred(
        &self,
        task_id: &str,
        run_at: DateTime<Utc>,
        reason: &str,
    ) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        task.status = TaskStatus::Pending;
        task.run_at = Some(run_at);
        task.updated_at = chrono::Utc::now();

        let task = self.repo.update(&task).await?;
        self.broadcast(&task, "deferred", Some(reason));
        Ok(task)
    }

    pub async fn mark_cancelled(&self, task_id: &str) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        task.status = TaskStatus::Cancelled;
        task.updated_at = chrono::Utc::now();

        let task = self.repo.update(&task).await?;
        self.broadcast(&task, "cancelled", None);
        Ok(task)
    }

    /// Idempotent for terminal states. Cascade-marks in-flight CronRun children
    /// when called on a Cron template.
    pub async fn cancel(&self, user_id: &str, task_id: &str) -> Result<Task, AppError> {
        let task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        if task.user_id != user_id {
            return Err(AppError::Forbidden("Not your task".into()));
        }

        if matches!(
            task.status,
            TaskStatus::Cancelled | TaskStatus::Completed | TaskStatus::Failed
        ) {
            return Ok(task);
        }

        if matches!(task.kind, TaskKind::Cron { .. }) {
            let active = self
                .find_active_runs_by_cron(task_id)
                .await
                .unwrap_or_default();
            for run in active {
                let _ = self.mark_cancelled(&run.id).await;
            }
        }

        self.mark_cancelled(task_id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_cron_template(
        &self,
        user_id: &str,
        agent_id: &str,
        title: &str,
        description: &str,
        cron_expression: &str,
        timezone: String,
        next_run_at: DateTime<Utc>,
        space_id: Option<String>,
        source_agent_id: Option<String>,
        source_chat_id: Option<String>,
        run_at: Option<DateTime<Utc>>,
        mode: super::models::CronMode,
        concurrency: super::models::CronConcurrency,
        process_result: bool,
        result_schema: Option<serde_json::Value>,
        result_description: Option<String>,
    ) -> Result<Task, AppError> {
        if let Some(ref schema) = result_schema {
            super::schema::validate_schema_doc(schema).map_err(AppError::Validation)?;
        }
        let now = chrono::Utc::now();
        let task = Task {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            agent_id: agent_id.to_string(),
            space_id,
            chat_id: None,
            title: title.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            kind: TaskKind::Cron {
                cron_expression: cron_expression.to_string(),
                timezone: Some(timezone),
                next_run_at: Some(next_run_at),
                source_agent_id,
                source_chat_id,
                mode,
                concurrency,
                process_result,
            },
            run_at,
            result_summary: None,
            error_message: None,
            quarantined: false,
            result_schema,
            result_description,
            created_at: now,
            updated_at: now,
        };
        let task = self.repo.create(&task).await?;
        self.broadcast(&task, "pending", None);
        Ok(task)
    }

    pub async fn advance_cron_template(
        &self,
        task_id: &str,
        next_run_at: DateTime<Utc>,
    ) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Cron template not found".into()))?;

        if let TaskKind::Cron {
            next_run_at: ref mut nra,
            ..
        } = task.kind
        {
            *nra = Some(next_run_at);
        }

        task.updated_at = chrono::Utc::now();

        let task = self.repo.update(&task).await?;
        self.broadcast(
            &task,
            status_str(&task.status),
            task.result_summary.as_deref(),
        );
        Ok(task)
    }

    pub async fn find_deferred_due(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_deferred_due(chrono::Utc::now()).await
    }

    pub async fn find_due_cron_templates(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_due_cron_templates(chrono::Utc::now()).await
    }

    /// The returned task has `chat_id = None`; `TaskExecutor::ensure_task_chat`
    /// mints a fresh per-fire chat when the run is spawned.
    pub async fn spawn_cron_run(
        &self,
        template: &Task,
        fire_at: DateTime<Utc>,
        sequence_num: u64,
    ) -> Result<Task, AppError> {
        let (source_agent_id, source_chat_id) = match &template.kind {
            TaskKind::Cron {
                source_agent_id,
                source_chat_id,
                ..
            } => (source_agent_id.clone(), source_chat_id.clone()),
            _ => {
                return Err(AppError::Internal(
                    "spawn_cron_run requires a Cron template task".into(),
                ));
            }
        };

        let now = chrono::Utc::now();
        let run = Task {
            id: crate::core::repository::new_id(),
            user_id: template.user_id.clone(),
            agent_id: template.agent_id.clone(),
            space_id: template.space_id.clone(),
            chat_id: None,
            title: template.title.clone(),
            description: template.description.clone(),
            status: TaskStatus::Pending,
            kind: TaskKind::CronRun {
                source_cron_id: template.id.clone(),
                source_chat_id,
                source_agent_id,
                fire_at,
                sequence_num,
            },
            run_at: None,
            result_summary: None,
            error_message: None,
            quarantined: false,
            result_schema: template.result_schema.clone(),
            result_description: template.result_description.clone(),
            created_at: now,
            updated_at: now,
        };
        let run = self.repo.create(&run).await?;
        self.broadcast(&run, "pending", None);
        Ok(run)
    }

    pub async fn find_runs_by_cron(&self, cron_id: &str) -> Result<Vec<Task>, AppError> {
        self.repo.find_runs_by_cron(cron_id).await
    }

    pub async fn find_active_runs_by_cron(&self, cron_id: &str) -> Result<Vec<Task>, AppError> {
        self.repo.find_active_runs_by_cron(cron_id).await
    }

    pub async fn find_orphaned_cron_runs(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_orphaned_cron_runs().await
    }
}
