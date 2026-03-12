use async_trait::async_trait;
use crate::chat::models::Chat;
use crate::chat::repository::ChatRepository;
use crate::core::error::AppError;

use super::generic::SurrealRepo;

pub type SurrealChatRepo = SurrealRepo<Chat>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl ChatRepository for SurrealRepo<Chat> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Chat>, AppError> {
        let query =
            format!("{SELECT_CLAUSE} FROM chat WHERE user_id = $user_id AND archived_at IS NONE ORDER BY updated_at DESC");
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let chats: Vec<Chat> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(chats)
    }

    async fn find_by_space_id(&self, space_id: &str) -> Result<Vec<Chat>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM chat WHERE space_id = $space_id AND archived_at IS NONE ORDER BY updated_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("space_id", space_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let chats: Vec<Chat> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(chats)
    }

    async fn find_standalone_by_user_id(&self, user_id: &str) -> Result<Vec<Chat>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM chat WHERE user_id = $user_id AND space_id IS NONE AND task_id IS NONE AND archived_at IS NONE ORDER BY updated_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let chats: Vec<Chat> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(chats)
    }

    async fn find_archived_by_user_id(&self, user_id: &str) -> Result<Vec<Chat>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM chat WHERE user_id = $user_id AND archived_at IS NOT NONE ORDER BY archived_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let chats: Vec<Chat> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(chats)
    }
}
