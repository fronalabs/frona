pub mod jwt;
pub mod models;
pub mod oauth;
pub mod token;

use async_trait::async_trait;

use self::models::{AuthResponse, LoginRequest, RegisterRequest, UpdateUsernameRequest, UserInfo};
use crate::auth::token::service::TokenService;
use crate::core::config::Config;
use crate::core::error::AppError;
use crate::core::models::User;
use crate::core::repository::Repository;
use crate::credential::keypair::service::KeyPairService;

#[async_trait]
pub trait UserRepository: Repository<User> {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, AppError>;
    async fn find_by_username(&self, username: &str) -> Result<Option<User>, AppError>;
}

#[derive(Default)]
pub struct AuthService;

impl AuthService {
    pub fn new() -> Self {
        Self
    }

    pub async fn register(
        &self,
        repo: &dyn UserRepository,
        keypair_svc: &KeyPairService,
        token_svc: &TokenService,
        req: RegisterRequest,
    ) -> Result<(AuthResponse, String), AppError> {
        Self::validate_username(&req.username)?;
        Self::validate_password(&req.password)?;

        if repo.find_by_email(&req.email).await?.is_some() {
            return Err(AppError::Validation("Email already registered".into()));
        }
        if repo.find_by_username(&req.username).await?.is_some() {
            return Err(AppError::Validation("Username already taken".into()));
        }

        let password_hash = self.hash_password(&req.password)?;
        let now = chrono::Utc::now();
        let user = User {
            id: uuid::Uuid::new_v4().to_string(),
            username: req.username,
            email: req.email,
            name: req.name,
            password_hash,
            created_at: now,
            updated_at: now,
        };

        let user = repo.create(&user).await?;
        let (access_jwt, refresh_jwt) =
            token_svc.create_session_pair(keypair_svc, &user).await?;

        let response = AuthResponse {
            token: access_jwt,
            user: UserInfo {
                id: user.id,
                username: user.username,
                email: user.email,
                name: user.name,
                needs_setup: None,
            },
        };

        Ok((response, refresh_jwt))
    }

    pub async fn login(
        &self,
        repo: &dyn UserRepository,
        keypair_svc: &KeyPairService,
        token_svc: &TokenService,
        req: LoginRequest,
    ) -> Result<(AuthResponse, String), AppError> {
        let user = if req.identifier.contains('@') {
            repo.find_by_email(&req.identifier).await?
        } else {
            repo.find_by_username(&req.identifier).await?
        }
        .ok_or_else(|| AppError::Auth("Invalid credentials".into()))?;

        self.verify_password(&req.password, &user.password_hash)?;
        let (access_jwt, refresh_jwt) =
            token_svc.create_session_pair(keypair_svc, &user).await?;

        let response = AuthResponse {
            token: access_jwt,
            user: UserInfo {
                id: user.id,
                username: user.username,
                email: user.email,
                name: user.name,
                needs_setup: None,
            },
        };

        Ok((response, refresh_jwt))
    }

    pub fn validate_password(password: &str) -> Result<(), AppError> {
        if password.len() < 8 {
            return Err(AppError::Validation(
                "Password must be at least 8 characters".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_username(username: &str) -> Result<(), AppError> {
        if username.len() < 2 || username.len() > 32 {
            return Err(AppError::Validation(
                "Username must be 2-32 characters".into(),
            ));
        }
        if !username.starts_with(|c: char| c.is_ascii_lowercase()) {
            return Err(AppError::Validation(
                "Username must start with a lowercase letter".into(),
            ));
        }
        if !username
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(AppError::Validation(
                "Username may only contain lowercase letters, digits, hyphens, and underscores"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn derive_username_from_email(email: &str) -> String {
        let prefix = email.split('@').next().unwrap_or(email);
        prefix
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_start_matches(|c: char| !c.is_ascii_lowercase())
            .to_string()
    }

    pub async fn generate_unique_username(
        repo: &dyn UserRepository,
        base: &str,
    ) -> Result<String, AppError> {
        let base = if base.is_empty() || !base.starts_with(|c: char| c.is_ascii_lowercase()) {
            format!("u-{base}")
        } else {
            base.to_string()
        };

        let truncated = if base.len() > 29 { &base[..29] } else { &base };

        if repo.find_by_username(truncated).await?.is_none() {
            return Ok(truncated.to_string());
        }
        for i in 2..1000 {
            let candidate = format!("{truncated}-{i}");
            if candidate.len() <= 32 && repo.find_by_username(&candidate).await?.is_none() {
                return Ok(candidate);
            }
        }
        Err(AppError::Internal("Could not generate unique username".into()))
    }

    pub async fn change_username(
        &self,
        repo: &dyn UserRepository,
        keypair_svc: &KeyPairService,
        token_svc: &TokenService,
        config: &Config,
        user_id: &str,
        req: UpdateUsernameRequest,
    ) -> Result<(AuthResponse, String), AppError> {
        Self::validate_username(&req.username)?;

        if repo.find_by_username(&req.username).await?.is_some() {
            return Err(AppError::Validation("Username already taken".into()));
        }

        let mut user = repo
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;

        let old_username = user.username.clone();
        if old_username == req.username {
            return Err(AppError::Validation("Username is the same".into()));
        }

        user.username = req.username.clone();
        user.updated_at = chrono::Utc::now();
        repo.update(&user).await?;

        // Rename files directory
        let old_files_dir = std::path::Path::new(&config.storage.files_path).join(&old_username);
        let new_files_dir = std::path::Path::new(&config.storage.files_path).join(&req.username);
        if old_files_dir.exists() {
            tokio::fs::rename(&old_files_dir, &new_files_dir)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to rename files directory: {e}")))?;
        }

        // Rename browser profiles directory
        if let Some(browser) = &config.browser {
            let old_profiles_dir = std::path::Path::new(&browser.profiles_path).join(&old_username);
            let new_profiles_dir = std::path::Path::new(&browser.profiles_path).join(&req.username);
            if old_profiles_dir.exists() {
                tokio::fs::rename(&old_profiles_dir, &new_profiles_dir)
                    .await
                    .map_err(|e| AppError::Internal(format!("Failed to rename profiles directory: {e}")))?;
            }
        }

        // Revoke all active tokens (force re-login)
        let tokens = token_svc.repo().find_by_user_id(user_id).await?;
        for token in &tokens {
            let _ = token_svc.repo().delete(&token.id).await;
        }

        // Create new session pair
        let (access_jwt, refresh_jwt) =
            token_svc.create_session_pair(keypair_svc, &user).await?;

        let response = AuthResponse {
            token: access_jwt,
            user: UserInfo {
                id: user.id,
                username: user.username,
                email: user.email,
                name: user.name,
                needs_setup: None,
            },
        };

        Ok((response, refresh_jwt))
    }

    fn hash_password(&self, password: &str) -> Result<String, AppError> {
        use argon2::{
            Argon2,
            password_hash::{PasswordHasher, SaltString, rand_core::OsRng},
        };
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        argon2
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| AppError::Internal(format!("Password hashing failed: {e}")))
    }

    fn verify_password(&self, password: &str, hash: &str) -> Result<(), AppError> {
        use argon2::{
            Argon2,
            password_hash::{PasswordHash, PasswordVerifier},
        };
        let parsed = PasswordHash::new(hash)
            .map_err(|e| AppError::Internal(format!("Invalid password hash: {e}")))?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AppError::Auth("Invalid email or password".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_password_accepts_8_chars() {
        assert!(AuthService::validate_password("12345678").is_ok());
    }

    #[test]
    fn validate_password_accepts_long_password() {
        assert!(AuthService::validate_password("a-long-secure-passphrase!").is_ok());
    }

    #[test]
    fn validate_password_rejects_short_password() {
        let err = AuthService::validate_password("abc").unwrap_err();
        assert!(matches!(err, AppError::Validation(_)));
    }

    #[test]
    fn validate_password_rejects_7_chars() {
        assert!(AuthService::validate_password("1234567").is_err());
    }
}
