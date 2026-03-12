use async_trait::async_trait;

use crate::contact::models::Contact;
use crate::contact::repository::ContactRepository;
use crate::core::error::AppError;

use super::generic::SurrealRepo;

const SELECT_CLAUSE: &str = "SELECT *, meta::id(id) as id";

#[async_trait]
impl ContactRepository for SurrealRepo<Contact> {
    async fn find_by_user_id(&self, user_id: &str) -> Result<Vec<Contact>, AppError> {
        let query =
            format!("{SELECT_CLAUSE} FROM contact WHERE user_id = $user_id ORDER BY created_at DESC");
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let contacts: Vec<Contact> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(contacts)
    }

    async fn find_by_phone(&self, user_id: &str, phone: &str) -> Result<Option<Contact>, AppError> {
        let query = format!(
            "{SELECT_CLAUSE} FROM contact WHERE user_id = $user_id AND phone = $phone LIMIT 1"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .bind(("phone", phone.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let contact: Option<Contact> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(contact)
    }
}
