use async_trait::async_trait;

use crate::core::error::AppError;
use crate::core::repository::Repository;

use super::models::{Credential, VaultAccessLog, VaultConnection, VaultGrant};

#[async_trait]
pub trait CredentialRepository: Repository<Credential> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Credential>, AppError>;
    async fn find_by_user_and_provider(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<Option<Credential>, AppError>;
}

#[async_trait]
pub trait VaultConnectionRepository: Repository<VaultConnection> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<VaultConnection>, AppError>;
    async fn find_all_for_user(&self, user_id: &str) -> Result<Vec<VaultConnection>, AppError>;
    async fn find_system_managed(&self) -> Result<Vec<VaultConnection>, AppError>;
}

#[async_trait]
pub trait VaultGrantRepository: Repository<VaultGrant> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<VaultGrant>, AppError>;
    async fn find_matching_grant(
        &self,
        user_id: &str,
        agent_id: &str,
        query: &str,
        env_var_prefix: Option<&str>,
    ) -> Result<Option<VaultGrant>, AppError>;
    async fn delete_by_connection_id(&self, connection_id: &str) -> Result<(), AppError>;
}

#[async_trait]
pub trait VaultAccessLogRepository: Repository<VaultAccessLog> {
    async fn find_by_chat_id(&self, chat_id: &str) -> Result<Vec<VaultAccessLog>, AppError>;
    async fn find_by_chat_and_query(
        &self,
        chat_id: &str,
        query: &str,
        env_var_prefix: Option<&str>,
    ) -> Result<Option<VaultAccessLog>, AppError>;
}
