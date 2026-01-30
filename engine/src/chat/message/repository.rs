use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::error::AppError;
use crate::repository::Repository;

use super::models::Message;

#[async_trait]
pub trait MessageRepository: Repository<Message> {
    async fn find_by_chat_id(&self, chat_id: &str) -> Result<Vec<Message>, AppError>;
    async fn find_by_chat_id_after(
        &self,
        chat_id: &str,
        after: DateTime<Utc>,
    ) -> Result<Vec<Message>, AppError>;
    async fn delete_by_chat_id_before(
        &self,
        chat_id: &str,
        before: DateTime<Utc>,
    ) -> Result<(), AppError>;
}
