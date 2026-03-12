use async_trait::async_trait;
use crate::core::error::AppError;
use crate::space::models::Space;
use crate::space::repository::SpaceRepository;

use super::generic::SurrealRepo;

pub type SurrealSpaceRepo = SurrealRepo<Space>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl SpaceRepository for SurrealRepo<Space> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Space>, AppError> {
        let query =
            format!("{SELECT_CLAUSE} FROM space WHERE user_id = $user_id ORDER BY created_at DESC");
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let spaces: Vec<Space> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(spaces)
    }

    async fn find_all(&self) -> Result<Vec<Space>, AppError> {
        let query = format!("{SELECT_CLAUSE} FROM space ORDER BY created_at DESC");
        let mut result = self
            .db()
            .query(&query)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let spaces: Vec<Space> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(spaces)
    }
}
