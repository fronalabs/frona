use crate::api::repo::tasks::SurrealTaskRepo;
use crate::error::AppError;
use crate::repository::Repository;

use super::dto::{CreateTaskRequest, TaskResponse, UpdateTaskRequest};
use super::models::{Task, TaskStatus};
use super::repository::TaskRepository;

#[derive(Clone)]
pub struct TaskService {
    repo: SurrealTaskRepo,
}

impl TaskService {
    pub fn new(repo: SurrealTaskRepo) -> Self {
        Self { repo }
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateTaskRequest,
    ) -> Result<TaskResponse, AppError> {
        let now = chrono::Utc::now();
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            chat_id: req.chat_id,
            title: req.title,
            description: req.description.unwrap_or_default(),
            status: TaskStatus::Pending,
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
}
