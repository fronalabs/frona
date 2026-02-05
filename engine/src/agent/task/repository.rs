use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::error::AppError;
use crate::repository::Repository;

use super::models::Task;

#[async_trait]
pub trait TaskRepository: Repository<Task> {
    async fn find_active_by_user_id(&self, user_id: &str) -> Result<Vec<Task>, AppError>;
    async fn find_all_by_user_id(&self, user_id: &str) -> Result<Vec<Task>, AppError>;
    async fn find_resumable(&self) -> Result<Vec<Task>, AppError>;
    async fn find_by_chat_id(&self, chat_id: &str) -> Result<Option<Task>, AppError>;
    async fn find_by_source_chat_id(&self, source_chat_id: &str) -> Result<Vec<Task>, AppError>;
    async fn find_due_cron_templates(&self, now: DateTime<Utc>) -> Result<Vec<Task>, AppError>;
    async fn find_deferred_due(&self, now: DateTime<Utc>) -> Result<Vec<Task>, AppError>;
}
