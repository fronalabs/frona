use chrono::Utc;
use uuid::Uuid;

use crate::db::repo::generic::SurrealRepo;
use crate::core::error::AppError;
use crate::core::repository::Repository;

use super::models::{Contact, ContactResponse, CreateContactRequest, UpdateContactRequest};
use super::repository::ContactRepository;

#[derive(Clone)]
pub struct ContactService {
    repo: SurrealRepo<Contact>,
}

impl ContactService {
    pub fn new(repo: SurrealRepo<Contact>) -> Self {
        Self { repo }
    }

    pub async fn find_or_create_by_phone(
        &self,
        user_id: &str,
        phone: &str,
        name: &str,
    ) -> Result<ContactResponse, AppError> {
        if let Some(c) = self.repo.find_by_phone(user_id, phone).await? {
            return Ok(c.into());
        }
        let now = Utc::now();
        let contact = Contact {
            id: Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: name.to_string(),
            phone: Some(phone.to_string()),
            email: None,
            company: None,
            job_title: None,
            notes: None,
            avatar: None,
            created_at: now,
            updated_at: now,
        };
        Ok(self.repo.create(&contact).await?.into())
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<ContactResponse>, AppError> {
        let contacts = self.repo.find_by_user_id(user_id).await?;
        Ok(contacts.into_iter().map(Into::into).collect())
    }

    pub async fn get(&self, user_id: &str, contact_id: &str) -> Result<ContactResponse, AppError> {
        let contact = self
            .repo
            .find_by_id(contact_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Contact not found".into()))?;
        if contact.user_id != user_id {
            return Err(AppError::Forbidden("Not your contact".into()));
        }
        Ok(contact.into())
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateContactRequest,
    ) -> Result<ContactResponse, AppError> {
        let now = Utc::now();
        let contact = Contact {
            id: Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: req.name,
            phone: req.phone,
            email: req.email,
            company: req.company,
            job_title: req.job_title,
            notes: req.notes,
            avatar: req.avatar,
            created_at: now,
            updated_at: now,
        };
        Ok(self.repo.create(&contact).await?.into())
    }

    pub async fn update(
        &self,
        user_id: &str,
        contact_id: &str,
        req: UpdateContactRequest,
    ) -> Result<ContactResponse, AppError> {
        let mut contact = self
            .repo
            .find_by_id(contact_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Contact not found".into()))?;
        if contact.user_id != user_id {
            return Err(AppError::Forbidden("Not your contact".into()));
        }
        if let Some(name) = req.name {
            contact.name = name;
        }
        if let Some(phone) = req.phone {
            contact.phone = Some(phone);
        }
        if let Some(email) = req.email {
            contact.email = Some(email);
        }
        if let Some(company) = req.company {
            contact.company = Some(company);
        }
        if let Some(job_title) = req.job_title {
            contact.job_title = Some(job_title);
        }
        if let Some(notes) = req.notes {
            contact.notes = Some(notes);
        }
        if let Some(avatar) = req.avatar {
            contact.avatar = Some(avatar);
        }
        contact.updated_at = Utc::now();
        Ok(self.repo.update(&contact).await?.into())
    }

    pub async fn delete(&self, user_id: &str, contact_id: &str) -> Result<(), AppError> {
        let contact = self
            .repo
            .find_by_id(contact_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Contact not found".into()))?;
        if contact.user_id != user_id {
            return Err(AppError::Forbidden("Not your contact".into()));
        }
        self.repo.delete(contact_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_test_service() -> ContactService {
        use surrealdb::Surreal;
        use surrealdb::engine::local::Mem;
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        ContactService::new(SurrealRepo::new(db))
    }

    #[tokio::test]
    async fn find_or_create_by_phone_creates_new_contact() {
        let svc = make_test_service().await;

        let result = svc.find_or_create_by_phone("user-1", "+15555551234", "Alice").await.unwrap();
        assert_eq!(result.name, "Alice");
        assert_eq!(result.phone.as_deref(), Some("+15555551234"));
        assert_eq!(result.user_id, "user-1");
    }

    #[tokio::test]
    async fn find_or_create_by_phone_returns_existing() {
        let svc = make_test_service().await;

        let first = svc.find_or_create_by_phone("user-1", "+15555551234", "Alice").await.unwrap();
        let second = svc.find_or_create_by_phone("user-1", "+15555551234", "Alice2").await.unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.name, "Alice");
    }
}
