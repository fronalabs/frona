use chrono::{DateTime, Utc};

use crate::db::repo::tasks::SurrealTaskRepo;
use crate::core::error::AppError;
use crate::core::repository::Repository;

use super::models::{CreateTaskRequest, TaskResponse, UpdateTaskRequest};
use super::models::{Task, TaskKind, TaskStatus};
use super::repository::TaskRepository;

#[derive(Clone)]
pub struct TaskService {
    repo: SurrealTaskRepo,
}

impl TaskService {
    pub fn new(repo: SurrealTaskRepo) -> Self {
        Self { repo }
    }

    pub fn repo(&self) -> &SurrealTaskRepo {
        &self.repo
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
                deliver_directly: req.deliver_directly.unwrap_or(false),
            },
            _ => TaskKind::Direct,
        };

        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
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
            created_at: now,
            updated_at: now,
        };

        let task = self.repo.create(&task).await?;
        Ok(task.into())
    }

    pub async fn list_active(
        &self,
        user_id: &str,
    ) -> Result<Vec<TaskResponse>, AppError> {
        let tasks = self.repo.find_active_by_user_id(user_id).await?;
        Ok(tasks.into_iter().map(Into::into).collect())
    }

    pub async fn list_all(
        &self,
        user_id: &str,
    ) -> Result<Vec<TaskResponse>, AppError> {
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
        Ok(task.into())
    }

    pub async fn delete(
        &self,
        user_id: &str,
        task_id: &str,
    ) -> Result<(), AppError> {
        let task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        if task.user_id != user_id {
            return Err(AppError::Forbidden("Not your task".into()));
        }

        self.repo.delete(task_id).await
    }

    pub async fn find_resumable(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_resumable().await
    }

    pub async fn find_by_id(&self, task_id: &str) -> Result<Option<Task>, AppError> {
        self.repo.find_by_id(task_id).await
    }

    pub async fn find_by_chat_id(&self, chat_id: &str) -> Result<Option<Task>, AppError> {
        self.repo.find_by_chat_id(chat_id).await
    }

    pub async fn find_by_source_chat_id(&self, source_chat_id: &str) -> Result<Vec<Task>, AppError> {
        self.repo.find_by_source_chat_id(source_chat_id).await
    }

    pub async fn mark_in_progress(&self, task_id: &str, chat_id: Option<&str>) -> Result<Task, AppError> {
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

        self.repo.update(&task).await
    }

    pub async fn mark_completed(&self, task_id: &str, summary: Option<String>) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        task.status = TaskStatus::Completed;
        task.result_summary = summary;
        task.updated_at = chrono::Utc::now();

        self.repo.update(&task).await
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

        self.repo.update(&task).await
    }

    pub async fn mark_cancelled(&self, task_id: &str) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        task.status = TaskStatus::Cancelled;
        task.updated_at = chrono::Utc::now();

        self.repo.update(&task).await
    }

    pub async fn cancel(
        &self,
        user_id: &str,
        task_id: &str,
    ) -> Result<Task, AppError> {
        let task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Task not found".into()))?;

        if task.user_id != user_id {
            return Err(AppError::Forbidden("Not your task".into()));
        }

        match task.status {
            TaskStatus::Pending | TaskStatus::InProgress => {
                self.mark_cancelled(task_id).await
            }
            _ => Err(AppError::Validation(format!(
                "Cannot cancel task with status {:?}",
                task.status
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_cron_template(
        &self,
        user_id: &str,
        agent_id: &str,
        title: &str,
        description: &str,
        cron_expression: &str,
        next_run_at: DateTime<Utc>,
        source_agent_id: Option<String>,
        source_chat_id: Option<String>,
        run_at: Option<DateTime<Utc>>,
    ) -> Result<Task, AppError> {
        let now = chrono::Utc::now();
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            agent_id: agent_id.to_string(),
            space_id: None,
            chat_id: None,
            title: title.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            kind: TaskKind::Cron {
                cron_expression: cron_expression.to_string(),
                next_run_at: Some(next_run_at),
                source_agent_id,
                source_chat_id,
            },
            run_at,
            result_summary: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&task).await
    }

    pub async fn advance_cron_template(
        &self,
        task_id: &str,
        next_run_at: DateTime<Utc>,
        chat_id: Option<&str>,
    ) -> Result<Task, AppError> {
        let mut task = self
            .repo
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Cron template not found".into()))?;

        if let TaskKind::Cron { next_run_at: ref mut nra, .. } = task.kind {
            *nra = Some(next_run_at);
        }

        if let Some(cid) = chat_id {
            task.chat_id = Some(cid.to_string());
        }
        task.updated_at = chrono::Utc::now();

        self.repo.update(&task).await
    }

    pub async fn find_deferred_due(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_deferred_due(chrono::Utc::now()).await
    }

    pub async fn find_due_cron_templates(&self) -> Result<Vec<Task>, AppError> {
        self.repo.find_due_cron_templates(chrono::Utc::now()).await
    }
}
