use async_trait::async_trait;

use crate::error::AppError;
use crate::repository::Repository;

use super::models::Credential;

#[async_trait]
pub trait CredentialRepository: Repository<Credential> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Credential>, AppError>;
    async fn find_by_user_and_provider(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<Option<Credential>, AppError>;
}
