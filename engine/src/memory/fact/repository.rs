use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::AppError;
use crate::repository::Repository;

use super::models::Fact;

#[async_trait]
pub trait FactRepository: Repository<Fact> {
    async fn find_by_agent_id(&self, agent_id: &str) -> Result<Vec<Fact>, AppError>;
    async fn find_by_agent_id_after(
        &self,
        agent_id: &str,
        after: DateTime<Utc>,
    ) -> Result<Vec<Fact>, AppError>;
    async fn delete_by_agent_id_before(
        &self,
        agent_id: &str,
        before: DateTime<Utc>,
    ) -> Result<(), AppError>;
    async fn find_distinct_agent_ids(&self) -> Result<Vec<String>, AppError>;
}
