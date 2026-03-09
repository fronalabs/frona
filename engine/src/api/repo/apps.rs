use async_trait::async_trait;
use crate::app::models::{App, AppStatus};
use crate::app::repository::AppRepository;
use crate::core::error::AppError;

use super::generic::SurrealRepo;

pub type SurrealAppRepo = SurrealRepo<App>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl AppRepository for SurrealRepo<App> {
    async fn find_by_agent_id(&self, agent_id: &str) -> Result<Vec<App>, AppError> {
        let query =
            format!("{SELECT_CLAUSE} FROM app WHERE agent_id = $agent_id ORDER BY created_at DESC");
        let mut result = self
            .db()
            .query(&query)
            .bind(("agent_id", agent_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        result.take(0).map_err(|e| AppError::Database(e.to_string()))
    }

    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<App>, AppError> {
        let query =
            format!("{SELECT_CLAUSE} FROM app WHERE user_id = $user_id ORDER BY created_at DESC");
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        result.take(0).map_err(|e| AppError::Database(e.to_string()))
    }

    async fn find_running(&self) -> Result<Vec<App>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM app WHERE status IN $statuses"
        );
        let statuses = vec![
            AppStatus::Running,
            AppStatus::Hibernated,
            AppStatus::Serving,
        ];
        let mut result = self
            .db()
            .query(&query)
            .bind(("statuses", statuses))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        result.take(0).map_err(|e| AppError::Database(e.to_string()))
    }
}
