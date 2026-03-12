use async_trait::async_trait;

use crate::core::error::AppError;
use crate::credential::keypair::models::KeyPair;
use crate::credential::keypair::repository::KeyPairRepository;

use super::generic::SurrealRepo;

pub type SurrealKeyPairRepo = SurrealRepo<KeyPair>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl KeyPairRepository for SurrealRepo<KeyPair> {
    async fn find_active_by_owner(&self, owner: &str) -> Result<Option<KeyPair>, AppError> {
        let query =
            format!("{SELECT_CLAUSE} FROM keypair WHERE owner = $owner AND active = true LIMIT 1");
        let mut result = self
            .db()
            .query(&query)
            .bind(("owner", owner.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let kp: Option<KeyPair> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(kp)
    }

    async fn find_by_kid(&self, kid: &str) -> Result<Option<KeyPair>, AppError> {
        // kid is the owner string
        self.find_active_by_owner(kid).await
    }

    async fn find_all_active(&self) -> Result<Vec<KeyPair>, AppError> {
        let query = format!("{SELECT_CLAUSE} FROM keypair WHERE active = true");
        let mut result = self
            .db()
            .query(&query)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let keys: Vec<KeyPair> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(keys)
    }
}
