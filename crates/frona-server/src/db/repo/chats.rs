use crate::chat::models::Chat;
use crate::chat::repository::ChatRepository;
use crate::core::error::AppError;
use async_trait::async_trait;

use super::generic::SurrealRepo;

pub type SurrealChatRepo = SurrealRepo<Chat>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl ChatRepository for SurrealRepo<Chat> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Chat>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM chat WHERE user_id = $user_id AND archived_at IS NONE ORDER BY updated_at DESC"
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

    async fn find_all_by_space_id(&self, space_id: &str) -> Result<Vec<Chat>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM chat WHERE space_id = $space_id ORDER BY updated_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("space_id", space_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))
    }

    async fn find_user_chats_by_space_id(&self, space_id: &str) -> Result<Vec<Chat>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM chat WHERE space_id = $space_id AND task_id IS NONE AND archived_at IS NONE ORDER BY updated_at DESC"
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

    async fn find_by_channel_thread(
        &self,
        channel_id: &str,
        channel_external_id: &str,
    ) -> Result<Option<Chat>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM chat WHERE channel_id = $channel_id AND channel_external_id = $thread LIMIT 1"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("channel_id", channel_id.to_string()))
            .bind(("thread", channel_external_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let chat: Option<Chat> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(chat)
    }
}

impl SurrealRepo<Chat> {
    /// Update source bookkeeping without replacing other chat fields.
    pub async fn save_agent_context_state(
        &self,
        chat_id: &str,
        state: &crate::agent::context::AgentContextState,
    ) -> Result<(), AppError> {
        self.db()
            .query("UPDATE type::record('chat', $id) SET agent_context_state = $state")
            .bind(("id", chat_id.to_owned()))
            .bind(("state", state.clone()))
            .await
            .map_err(|error| AppError::Database(error.to_string()))?
            .check()
            .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(())
    }
}
