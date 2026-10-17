use async_trait::async_trait;
use chrono::Utc;

use crate::auth::token::models::{ApiToken, TokenType};
use crate::auth::token::repository::TokenRepository;
use crate::core::error::AppError;

use super::generic::SurrealRepo;

pub type SurrealTokenRepo = SurrealRepo<ApiToken>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl TokenRepository for SurrealRepo<ApiToken> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<ApiToken>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM api_token WHERE user_id = $user_id ORDER BY created_at DESC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let tokens: Vec<ApiToken> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(tokens)
    }

    async fn find_active_by_id(&self, id: &str) -> Result<Option<ApiToken>, AppError> {
        let query =
            format!("{SELECT_CLAUSE} FROM api_token WHERE id = $id AND expires_at > $now LIMIT 1");
        let thing = surrealdb::types::RecordId::new("api_token", id);
        let mut result = self
            .db()
            .query(&query)
            .bind(("id", thing))
            .bind(("now", Utc::now()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let token: Option<ApiToken> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(token)
    }

    async fn find_by_refresh_pair(&self, pair_id: &str) -> Result<Vec<ApiToken>, AppError> {
        let query = format!("{SELECT_CLAUSE} FROM api_token WHERE refresh_pair_id = $pair_id");
        let mut result = self
            .db()
            .query(&query)
            .bind(("pair_id", pair_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let tokens: Vec<ApiToken> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(tokens)
    }

    async fn update_last_used(&self, id: &str) -> Result<(), AppError> {
        self.db()
            .query("UPDATE type::record('api_token', $id) SET last_used_at = $now")
            .bind(("id", id.to_string()))
            .bind(("now", Utc::now()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    async fn delete_expired(&self) -> Result<u64, AppError> {
        let mut result = self
            .db()
            .query("DELETE FROM api_token WHERE expires_at <= $now")
            .bind(("now", Utc::now()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        // SurrealDB doesn't return count on DELETE easily, just return 0
        let _: Option<serde_json::Value> = result.take(0).unwrap_or(None);
        Ok(0)
    }

    async fn consume_refresh_pair(&self, token_id: &str, pair_id: &str) -> Result<bool, AppError> {
        let tx = self
            .db()
            .clone()
            .begin()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let consumed = async {
            let mut result = tx
                .query(
                    "DELETE api_token WHERE id = $id AND refresh_pair_id = $pair_id
                     AND token_type = $refresh_type AND expires_at > $now
                     RETURN BEFORE",
                )
                .bind(("id", surrealdb::types::RecordId::new("api_token", token_id)))
                .bind(("pair_id", pair_id.to_owned()))
                .bind(("refresh_type", TokenType::Refresh))
                .bind(("now", Utc::now()))
                .await?
                .check()?;
            let deleted: Vec<surrealdb::types::Value> = result.take(0)?;
            if deleted.is_empty() {
                return Ok(false);
            }

            tx.query("DELETE api_token WHERE refresh_pair_id = $pair_id")
                .bind(("pair_id", pair_id.to_owned()))
                .await?
                .check()?;
            Ok::<_, surrealdb::Error>(true)
        }
        .await;

        match consumed {
            Ok(true) => {
                // SurrealDB 3.2.4's RocksDB engine tracks the deleted key for
                // optimistic conflict detection. Both contenders can see a row,
                // but only one can commit its deletion. Never report success
                // from RETURN alone: the commit may still fail with a conflict.
                tx.commit()
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(true)
            }
            Ok(false) => {
                tx.cancel()
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(false)
            }
            Err(error) => {
                let _ = tx.cancel().await;
                Err(AppError::Database(error.to_string()))
            }
        }
    }

    async fn delete_by_refresh_pair(&self, pair_id: &str) -> Result<(), AppError> {
        self.db()
            .query("DELETE FROM api_token WHERE refresh_pair_id = $pair_id")
            .bind(("pair_id", pair_id.to_string()))
            .await
            .and_then(|response| response.check())
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    async fn delete_by_user_id(&self, user_id: &str) -> Result<(), AppError> {
        self.db()
            .query("DELETE FROM api_token WHERE user_id = $uid")
            .bind(("uid", user_id.to_string()))
            .await
            .and_then(|response| response.check())
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }
}
