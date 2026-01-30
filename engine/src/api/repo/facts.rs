use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::AppError;
use crate::memory::fact::models::Fact;
use crate::memory::fact::repository::FactRepository;

use super::generic::SurrealRepo;

pub type SurrealFactRepo = SurrealRepo<Fact>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl FactRepository for SurrealRepo<Fact> {
    async fn find_by_agent_id(&self, agent_id: &str) -> Result<Vec<Fact>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM fact WHERE agent_id = $agent_id ORDER BY created_at ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("agent_id", agent_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let facts: Vec<Fact> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(facts)
    }

    async fn find_by_agent_id_after(
        &self,
        agent_id: &str,
        after: DateTime<Utc>,
    ) -> Result<Vec<Fact>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM fact WHERE agent_id = $agent_id AND created_at > $after ORDER BY created_at ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("agent_id", agent_id.to_string()))
            .bind(("after", after))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let facts: Vec<Fact> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(facts)
    }

    async fn delete_by_agent_id_before(
        &self,
        agent_id: &str,
        before: DateTime<Utc>,
    ) -> Result<(), AppError> {
        self.db()
            .query("DELETE FROM fact WHERE agent_id = $agent_id AND created_at <= $before")
            .bind(("agent_id", agent_id.to_string()))
            .bind(("before", before))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(())
    }

    async fn find_distinct_agent_ids(&self) -> Result<Vec<String>, AppError> {
        let mut result = self
            .db()
            .query("SELECT DISTINCT agent_id FROM fact")
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows: Vec<serde_json::Value> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let ids = rows
            .into_iter()
            .filter_map(|v| v.get("agent_id").and_then(|id| id.as_str().map(String::from)))
            .collect();

        Ok(ids)
    }
}
