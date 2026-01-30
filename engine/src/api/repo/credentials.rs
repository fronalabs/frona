use async_trait::async_trait;

use crate::credential::models::Credential;
use crate::credential::repository::CredentialRepository;
use crate::error::AppError;

use super::generic::SurrealRepo;

pub type SurrealCredentialRepo = SurrealRepo<Credential>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl CredentialRepository for SurrealRepo<Credential> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Credential>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM credential WHERE user_id = $user_id ORDER BY created_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let credentials: Vec<Credential> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(credentials)
    }

    async fn find_by_user_and_provider(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<Option<Credential>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM credential WHERE user_id = $user_id AND provider = $provider LIMIT 1"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .bind(("provider", provider.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let credential: Option<Credential> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(credential)
    }
}
