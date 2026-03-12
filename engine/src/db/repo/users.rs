use async_trait::async_trait;
use crate::auth::UserRepository;
use crate::core::error::AppError;
use crate::core::models::User;

use super::generic::SurrealRepo;

pub type SurrealUserRepo = SurrealRepo<User>;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl UserRepository for SurrealRepo<User> {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, AppError> {
        let mut result = self
            .db()
            .query(format!("{SELECT_CLAUSE} FROM user WHERE email = $email LIMIT 1"))
            .bind(("email", email.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let user: Option<User> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(user)
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<User>, AppError> {
        let mut result = self
            .db()
            .query(format!("{SELECT_CLAUSE} FROM user WHERE username = $username LIMIT 1"))
            .bind(("username", username.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let user: Option<User> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(user)
    }

}
