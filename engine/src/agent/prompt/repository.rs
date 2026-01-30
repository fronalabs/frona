use async_trait::async_trait;

use crate::error::AppError;
use crate::repository::Repository;

use super::models::Prompt;

#[async_trait]
pub trait PromptRepository: Repository<Prompt> {
    async fn find_by_agent_and_name(
        &self,
        agent_id: &str,
        name: &str,
    ) -> Result<Option<Prompt>, AppError>;
}
