use async_trait::async_trait;
use surrealdb::types::RecordId;

use crate::call::models::Call;
use crate::call::repository::CallRepository;
use crate::core::error::AppError;

use super::generic::SurrealRepo;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl CallRepository for SurrealRepo<Call> {
    async fn find_by_chat_id(&self, chat_id: &str) -> Result<Option<Call>, AppError> {
        let query = format!("{SELECT_CLAUSE} FROM call WHERE chat = $chat LIMIT 1");
        let mut result = self
            .db()
            .query(&query)
            .bind(("chat", RecordId::new("chat", chat_id)))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let call: Option<Call> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(call)
    }
}
