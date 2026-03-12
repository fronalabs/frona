use async_trait::async_trait;

use crate::core::error::AppError;
use crate::memory::models::{Memory, MemorySourceType};
use crate::memory::repository::MemoryRepository;

use super::generic::SurrealRepo;

pub type SurrealMemoryRepo = SurrealRepo<Memory>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl MemoryRepository for SurrealRepo<Memory> {
    async fn find_latest(
        &self,
        source_type: MemorySourceType,
        source_id: &str,
    ) -> Result<Option<Memory>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM memory WHERE source_type = $st AND source_id = $sid ORDER BY created_at DESC LIMIT 1"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("st", source_type))
            .bind(("sid", source_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let memory: Option<Memory> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(memory)
    }
}
