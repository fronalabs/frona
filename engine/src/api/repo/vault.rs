use async_trait::async_trait;
use chrono::Utc;

use crate::core::error::AppError;
use crate::credential::vault::models::{Credential, VaultAccessLog, VaultConnection, VaultGrant};
use crate::credential::vault::repository::{CredentialRepository, VaultAccessLogRepository, VaultConnectionRepository, VaultGrantRepository};

use super::generic::SurrealRepo;

pub type SurrealVaultConnectionRepo = SurrealRepo<VaultConnection>;
pub type SurrealVaultGrantRepo = SurrealRepo<VaultGrant>;
pub type SurrealCredentialRepo = SurrealRepo<Credential>;
pub type SurrealVaultAccessLogRepo = SurrealRepo<VaultAccessLog>;

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

#[async_trait]
impl VaultConnectionRepository for SurrealRepo<VaultConnection> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<VaultConnection>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM vault_connection WHERE user_id = $user_id AND system_managed = false ORDER BY created_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let connections: Vec<VaultConnection> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(connections)
    }

    async fn find_all_for_user(&self, user_id: &str) -> Result<Vec<VaultConnection>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM vault_connection WHERE user_id = $user_id OR system_managed = true ORDER BY system_managed DESC, created_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let connections: Vec<VaultConnection> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(connections)
    }

    async fn find_system_managed(&self) -> Result<Vec<VaultConnection>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM vault_connection WHERE system_managed = true"
        );
        let mut result = self
            .db()
            .query(&query)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let connections: Vec<VaultConnection> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(connections)
    }
}

#[async_trait]
impl VaultGrantRepository for SurrealRepo<VaultGrant> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<VaultGrant>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM vault_grant WHERE user_id = $user_id ORDER BY created_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let grants: Vec<VaultGrant> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(grants)
    }

    async fn find_matching_grant(
        &self,
        user_id: &str,
        agent_id: &str,
        query: &str,
        env_var_prefix: Option<&str>,
    ) -> Result<Option<VaultGrant>, AppError> {
        let now = Utc::now();
        let surreal_query = if env_var_prefix.is_some() {
            format!(
                "{SELECT_CLAUSE} FROM vault_grant \
                 WHERE user_id = $user_id \
                 AND agent_id = $agent_id \
                 AND (query = $query OR env_var_prefix = $env_var_prefix) \
                 AND (expires_at IS NONE OR expires_at > $now) \
                 LIMIT 1"
            )
        } else {
            format!(
                "{SELECT_CLAUSE} FROM vault_grant \
                 WHERE user_id = $user_id \
                 AND agent_id = $agent_id \
                 AND query = $query \
                 AND (expires_at IS NONE OR expires_at > $now) \
                 LIMIT 1"
            )
        };

        let mut db_query = self
            .db()
            .query(&surreal_query)
            .bind(("user_id", user_id.to_string()))
            .bind(("agent_id", agent_id.to_string()))
            .bind(("query", query.to_string()))
            .bind(("now", now));

        if let Some(prefix) = env_var_prefix {
            db_query = db_query.bind(("env_var_prefix", prefix.to_string()));
        }

        let mut result = db_query
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let grant: Option<VaultGrant> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(grant)
    }

    async fn delete_by_connection_id(&self, connection_id: &str) -> Result<(), AppError> {
        self.db()
            .query("DELETE FROM vault_grant WHERE connection_id = $connection_id")
            .bind(("connection_id", connection_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl VaultAccessLogRepository for SurrealRepo<VaultAccessLog> {
    async fn find_by_chat_id(&self, chat_id: &str) -> Result<Vec<VaultAccessLog>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM vault_access_log WHERE chat_id = $chat_id ORDER BY created_at ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("chat_id", chat_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let logs: Vec<VaultAccessLog> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(logs)
    }

    async fn find_by_chat_and_query(
        &self,
        chat_id: &str,
        query: &str,
        env_var_prefix: Option<&str>,
    ) -> Result<Option<VaultAccessLog>, AppError> {
        let surreal_query = if env_var_prefix.is_some() {
            format!(
                "{SELECT_CLAUSE} FROM vault_access_log \
                 WHERE chat_id = $chat_id \
                 AND (query = $query OR env_var_prefix = $env_var_prefix) \
                 ORDER BY created_at DESC LIMIT 1"
            )
        } else {
            format!(
                "{SELECT_CLAUSE} FROM vault_access_log \
                 WHERE chat_id = $chat_id AND query = $query \
                 ORDER BY created_at DESC LIMIT 1"
            )
        };

        let mut db_query = self
            .db()
            .query(&surreal_query)
            .bind(("chat_id", chat_id.to_string()))
            .bind(("query", query.to_string()));

        if let Some(prefix) = env_var_prefix {
            db_query = db_query.bind(("env_var_prefix", prefix.to_string()));
        }

        let mut result = db_query
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let log: Option<VaultAccessLog> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(log)
    }
}
