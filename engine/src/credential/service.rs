use crate::api::repo::credentials::SurrealCredentialRepo;
use crate::error::AppError;
use crate::repository::Repository;

use super::dto::{CreateCredentialRequest, CredentialResponse};
use super::models::{Credential, CredentialData};
use super::repository::CredentialRepository;

#[derive(Clone)]
pub struct CredentialService {
    repo: SurrealCredentialRepo,
}

impl CredentialService {
    pub fn new(repo: SurrealCredentialRepo) -> Self {
        Self { repo }
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateCredentialRequest,
    ) -> Result<CredentialResponse, AppError> {
        let now = chrono::Utc::now();

        let data = match req.data {
            CredentialData::BrowserProfile => CredentialData::BrowserProfile,
            CredentialData::UsernamePassword { username, password_encrypted } => {
                CredentialData::UsernamePassword {
                    username,
                    password_encrypted: encrypt_password(&password_encrypted),
                }
            }
        };

        let credential = Credential {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: req.name,
            provider: req.provider,
            data,
            created_at: now,
            updated_at: now,
        };

        let credential = self.repo.create(&credential).await?;
        Ok(credential.into())
    }

    pub async fn list(
        &self,
        user_id: &str,
    ) -> Result<Vec<CredentialResponse>, AppError> {
        let credentials = self.repo.find_by_user_id(user_id).await?;
        Ok(credentials.into_iter().map(Into::into).collect())
    }

    pub async fn find_by_user_and_provider(
        &self,
        user_id: &str,
        provider: &str,
    ) -> Result<Option<Credential>, AppError> {
        self.repo.find_by_user_and_provider(user_id, provider).await
    }

    pub async fn find_by_id(&self, id: &str) -> Result<Option<Credential>, AppError> {
        self.repo.find_by_id(id).await
    }

    pub async fn delete(
        &self,
        user_id: &str,
        credential_id: &str,
    ) -> Result<(), AppError> {
        let credential = self
            .repo
            .find_by_id(credential_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Credential not found".into()))?;

        if credential.user_id != user_id {
            return Err(AppError::Forbidden("Not your credential".into()));
        }

        self.repo.delete(credential_id).await
    }
}

fn encrypt_password(password: &str) -> String {
    use argon2::password_hash::SaltString;
    use argon2::{Argon2, PasswordHasher};

    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("Failed to hash password")
        .to_string()
}
