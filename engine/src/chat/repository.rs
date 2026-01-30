use async_trait::async_trait;
use crate::error::AppError;
use crate::repository::Repository;

use super::models::Chat;

#[async_trait]
pub trait ChatRepository: Repository<Chat> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Chat>, AppError>;
    async fn find_by_space_id(&self, space_id: &str) -> Result<Vec<Chat>, AppError>;
    async fn find_standalone_by_user_id(&self, user_id: &str) -> Result<Vec<Chat>, AppError>;
}
