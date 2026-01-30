use async_trait::async_trait;
use crate::error::AppError;
use crate::repository::Repository;

use super::models::Agent;

#[async_trait]
pub trait AgentRepository: Repository<Agent> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Agent>, AppError>;
}
