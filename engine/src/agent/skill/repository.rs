use async_trait::async_trait;

use crate::error::AppError;
use crate::repository::Repository;

use super::models::Skill;

#[async_trait]
pub trait SkillRepository: Repository<Skill> {
    async fn find_by_name(
        &self,
        agent_id: Option<&str>,
        name: &str,
    ) -> Result<Option<Skill>, AppError>;

    async fn find_by_agent(
        &self,
        agent_id: Option<&str>,
    ) -> Result<Vec<Skill>, AppError>;
}
