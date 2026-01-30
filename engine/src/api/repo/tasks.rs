use async_trait::async_trait;
use crate::error::AppError;
use crate::task::models::Task;
use crate::task::repository::TaskRepository;

use super::generic::SurrealRepo;

pub type SurrealTaskRepo = SurrealRepo<Task>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl TaskRepository for SurrealRepo<Task> {
    async fn find_active_by_user_id(&self, user_id: &str) -> Result<Vec<Task>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM task WHERE user_id = $user_id AND status IN ['pending', 'inprogress'] ORDER BY created_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let tasks: Vec<Task> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(tasks)
    }
}
