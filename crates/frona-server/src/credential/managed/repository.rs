use super::Status;
use crate::core::error::AppError;
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

/// Connection-owned encrypted persistence with atomic identity/version checks.
#[async_trait]
pub trait ManagedVaultRepository: Send + Sync {
    async fn ensure_available(&self, connection_id: &str) -> Result<(), AppError>;
    async fn delete_connection(&self, connection_id: &str, user_id: &str) -> Result<(), AppError>;
    async fn write(
        &self,
        connection_id: &str,
        row: Value,
        expected_version: Option<Uuid>,
    ) -> Result<(), AppError>;
    async fn delete_by_id(
        &self,
        connection_id: &str,
        id: Uuid,
        expected_version: Uuid,
    ) -> Result<(), AppError>;
    async fn row_by_id(&self, connection_id: &str, id: Uuid) -> Result<Option<Value>, AppError>;
    async fn list(&self, connection_id: &str) -> Result<Vec<Status>, AppError>;
}
