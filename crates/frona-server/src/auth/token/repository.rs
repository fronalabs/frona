use async_trait::async_trait;

use super::models::ApiToken;
use crate::core::error::AppError;
use crate::core::repository::Repository;

#[async_trait]
pub trait TokenRepository: Repository<ApiToken> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<ApiToken>, AppError>;
    async fn find_active_by_id(&self, id: &str) -> Result<Option<ApiToken>, AppError>;
    async fn find_by_refresh_pair(&self, pair_id: &str) -> Result<Vec<ApiToken>, AppError>;
    async fn update_last_used(&self, id: &str) -> Result<(), AppError>;
    async fn delete_expired(&self) -> Result<u64, AppError>;
    /// Atomically consume an active refresh token and its pair. Only a committed
    /// consumer may create a replacement session; missing tokens return false.
    async fn consume_refresh_pair(&self, token_id: &str, pair_id: &str) -> Result<bool, AppError>;
    async fn delete_by_refresh_pair(&self, pair_id: &str) -> Result<(), AppError>;
    async fn delete_by_user_id(&self, user_id: &str) -> Result<(), AppError>;
}
