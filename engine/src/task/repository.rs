use async_trait::async_trait;
use crate::error::AppError;
use crate::repository::Repository;

use super::models::Task;

#[async_trait]
pub trait TaskRepository: Repository<Task> {
    async fn find_active_by_user_id(&self, user_id: &str) -> Result<Vec<Task>, AppError>;
}
