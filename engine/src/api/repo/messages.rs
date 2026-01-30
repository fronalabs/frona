use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::error::AppError;
use crate::chat::message::models::Message;
use crate::chat::message::repository::MessageRepository;

use super::generic::SurrealRepo;

pub type SurrealMessageRepo = SurrealRepo<Message>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl MessageRepository for SurrealRepo<Message> {
    async fn find_by_chat_id(&self, chat_id: &str) -> Result<Vec<Message>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM message WHERE chat_id = $chat_id ORDER BY created_at ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("chat_id", chat_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let messages: Vec<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(messages)
    }

    async fn find_by_chat_id_after(
        &self,
        chat_id: &str,
        after: DateTime<Utc>,
    ) -> Result<Vec<Message>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM message WHERE chat_id = $chat_id AND created_at > $after ORDER BY created_at ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("chat_id", chat_id.to_string()))
            .bind(("after", after))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let messages: Vec<Message> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(messages)
    }

    async fn delete_by_chat_id_before(
        &self,
        chat_id: &str,
        before: DateTime<Utc>,
    ) -> Result<(), AppError> {
        self.db()
            .query("DELETE FROM message WHERE chat_id = $chat_id AND created_at <= $before")
            .bind(("chat_id", chat_id.to_string()))
            .bind(("before", before))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(())
    }
}
