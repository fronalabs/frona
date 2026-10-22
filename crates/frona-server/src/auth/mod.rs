pub mod ephemeral_token;
pub mod group_service;
pub mod jwt;
pub mod lockout;
pub mod models;
pub mod oauth;
pub mod token;
pub mod user_service;

use async_trait::async_trait;

pub use self::models::User;
use self::models::{
    ADMINS_GROUP, AuthResponse, LoginRequest, RegisterRequest, UpdateHandleRequest,
    UpdateProfileRequest, UserInfo, UserPermissions,
};
pub use self::user_service::UserService;
use crate::auth::token::service::TokenService;
use crate::core::config::Config;
use crate::core::error::{AppError, AuthErrorCode};
use crate::core::repository::Repository;
use crate::credential::keypair::service::KeyPairService;
use crate::policy::models::PolicyAction;
use crate::policy::service::PolicyService;

/// Every endpoint returning a `UserInfo` must go through here. Constructing
/// one inline with `UserPermissions::default()` silently lies about admin
/// status until the frontend re-fetches `/me`.
pub async fn build_user_info(
    user: User,
    policy_service: &PolicyService,
    needs_setup: Option<bool>,
) -> Result<UserInfo, AppError> {
    let list_users = policy_service
        .authorize_user(&user, PolicyAction::ListUsers)
        .await?;
    let is_admin = user.groups.iter().any(|g| g == ADMINS_GROUP);
    Ok(UserInfo {
        id: user.id,
        handle: user.handle,
        email: user.email,
        name: user.name,
        timezone: user.timezone,
        needs_setup,
        permissions: UserPermissions {
            list_users: list_users.allowed,
            is_admin,
        },
    })
}

#[async_trait]
pub trait UserRepository: Repository<User> {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, AppError>;
    async fn find_by_handle(&self, handle: &crate::core::Handle) -> Result<Option<User>, AppError>;
    async fn has_users(&self) -> Result<bool, AppError>;
    async fn find_any_active_admin(&self) -> Result<Option<User>, AppError>;
    async fn find_oldest_active(&self) -> Result<Option<User>, AppError>;
    async fn list_all(&self, include_deactivated: bool) -> Result<Vec<User>, AppError>;
}

pub fn can_create_users(config: &Config) -> bool {
    (config.auth.allow_registration && !config.sso.disable_local_auth) || config.sso.enabled
}

#[derive(Default)]
pub struct AuthService;

impl AuthService {
    pub fn new() -> Self {
        Self
    }

    pub async fn create_user_with_password(
        &self,
        user_service: &UserService,
        req: RegisterRequest,
        groups: Vec<String>,
    ) -> Result<User, AppError> {
        let handle = crate::core::Handle::try_new(req.handle)?;
        Self::validate_password(&req.password)?;

        let email = Self::normalize_email(&req.email);
        if user_service.find_by_email(&email).await?.is_some() {
            return Err(AppError::Validation("Email already registered".into()));
        }
        if user_service.find_by_handle(&handle).await?.is_some() {
            return Err(AppError::Validation("Username already taken".into()));
        }

        let password_hash = self.hash_password(&req.password)?;
        let now = chrono::Utc::now();
        let user = User {
            id: crate::core::repository::new_id(),
            handle,
            email,
            name: req.name,
            password_hash,
            timezone: None,
            groups,
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        };
        let user = user_service.create(&user).await?;
        user_service.ensure_admin_invariant().await?;
        // Re-read so the caller sees any admin promotion.
        Ok(user_service.find_by_id(&user.id).await?.unwrap_or(user))
    }

    pub async fn register(
        &self,
        user_service: &UserService,
        keypair_svc: &KeyPairService,
        token_svc: &TokenService,
        policy_service: &PolicyService,
        req: RegisterRequest,
    ) -> Result<(AuthResponse, String), AppError> {
        let user = self
            .create_user_with_password(user_service, req, Vec::new())
            .await?;
        let (access_jwt, refresh_jwt) = token_svc.create_session_pair(keypair_svc, &user).await?;

        let response = AuthResponse {
            token: access_jwt,
            user: build_user_info(user, policy_service, None).await?,
        };

        Ok((response, refresh_jwt))
    }

    pub async fn login(
        &self,
        user_service: &UserService,
        keypair_svc: &KeyPairService,
        token_svc: &TokenService,
        policy_service: &PolicyService,
        req: LoginRequest,
    ) -> Result<(AuthResponse, String), AppError> {
        let user = if req.identifier.contains('@') {
            user_service.find_by_email(&req.identifier).await?
        } else {
            // Invalid handle → no such user.
            match crate::core::Handle::try_new(req.identifier.clone()) {
                Ok(h) => user_service.find_by_handle(&h).await?,
                Err(_) => None,
            }
        }
        .ok_or_else(|| AppError::Auth {
            message: "Invalid credentials".into(),
            code: AuthErrorCode::InvalidCredentials,
        })?;

        if user.deactivated_at.is_some() {
            return Err(AppError::Auth {
                message: "Account deactivated".into(),
                code: AuthErrorCode::AccountDeactivated,
            });
        }

        self.verify_password(&req.password, &user.password_hash)?;
        let (access_jwt, refresh_jwt) = token_svc.create_session_pair(keypair_svc, &user).await?;

        let response = AuthResponse {
            token: access_jwt,
            user: build_user_info(user, policy_service, None).await?,
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

    pub fn normalize_email(email: &str) -> String {
        email.trim().to_lowercase()
    }

    pub fn derive_handle_from_email(email: &str) -> String {
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

    pub async fn generate_unique_handle(
        user_service: &UserService,
        base: &str,
    ) -> Result<crate::core::Handle, AppError> {
        let base = if base.is_empty() || !base.starts_with(|c: char| c.is_ascii_lowercase()) {
            format!("u-{base}")
        } else {
            base.to_string()
        };

        let truncated = if base.len() > 29 { &base[..29] } else { &base };

        if let Ok(h) = crate::core::Handle::try_new(truncated)
            && user_service.find_by_handle(&h).await?.is_none()
        {
            return Ok(h);
        }
        for i in 2..1000 {
            let candidate = format!("{truncated}-{i}");
            if candidate.len() > 32 {
                continue;
            }
            let Ok(h) = crate::core::Handle::try_new(&candidate) else {
                continue;
            };
            if user_service.find_by_handle(&h).await?.is_none() {
                return Ok(h);
            }
        }
        Err(AppError::Internal(
            "Could not generate unique handle".into(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn change_handle(
        &self,
        user_service: &UserService,
        keypair_svc: &KeyPairService,
        token_svc: &TokenService,
        policy_service: &PolicyService,
        storage: &crate::storage::service::StorageService,
        config: &Config,
        user_id: &str,
        req: UpdateHandleRequest,
    ) -> Result<(AuthResponse, String), AppError> {
        let new_handle = crate::core::Handle::try_new(req.handle)?;

        if user_service.find_by_handle(&new_handle).await?.is_some() {
            return Err(AppError::Validation("Username already taken".into()));
        }

        let mut user = user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;

        let old_handle = user.handle.clone();
        if old_handle == new_handle {
            return Err(AppError::Validation("Username is the same".into()));
        }

        user.handle = new_handle.clone();
        user.updated_at = chrono::Utc::now();
        user_service.update(&user).await?;

        // Single rename moves every per-user subsystem atomically.
        let old_user_root = storage.user_root(&old_handle);
        let new_user_root = storage.user_root(&new_handle);
        if old_user_root.exists() {
            tokio::fs::rename(&old_user_root, &new_user_root)
                .await
                .map_err(|e| {
                    AppError::Internal(format!("Failed to rename user data directory: {e}"))
                })?;
        }

        // Browser profiles dir is a Docker volume mount, not under user_root.
        if let Some(browser) = &config.browser {
            let old_profiles_dir =
                std::path::Path::new(&browser.profiles_path).join(old_handle.as_ref());
            let new_profiles_dir =
                std::path::Path::new(&browser.profiles_path).join(new_handle.as_ref());
            if old_profiles_dir.exists() {
                tokio::fs::rename(&old_profiles_dir, &new_profiles_dir)
                    .await
                    .map_err(|e| {
                        AppError::Internal(format!("Failed to rename profiles directory: {e}"))
                    })?;
            }
        }

        let tokens = token_svc.repo().find_by_user_id(user_id).await?;
        for token in &tokens {
            let _ = token_svc.repo().delete(&token.id).await;
        }

        let (access_jwt, refresh_jwt) = token_svc.create_session_pair(keypair_svc, &user).await?;

        let response = AuthResponse {
            token: access_jwt,
            user: build_user_info(user, policy_service, None).await?,
        };

        Ok((response, refresh_jwt))
    }

    pub async fn update_profile(
        &self,
        user_service: &UserService,
        policy_service: &PolicyService,
        user_id: &str,
        req: UpdateProfileRequest,
    ) -> Result<UserInfo, AppError> {
        let mut user = user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;

        if let Some(ref tz) = req.timezone
            && !tz.is_empty()
            && tz.parse::<chrono_tz::Tz>().is_err()
        {
            return Err(AppError::Validation(format!(
                "Invalid timezone '{}'. Use an IANA name like 'America/Los_Angeles', 'Asia/Tokyo', or 'UTC'.",
                tz
            )));
        }

        user.timezone = req.timezone.filter(|s| !s.is_empty());
        user.updated_at = chrono::Utc::now();
        user_service.update(&user).await?;

        build_user_info(user, policy_service, None).await
    }

    fn hash_password(&self, password: &str) -> Result<String, AppError> {
        use argon2::{Argon2, password_hash::PasswordHasher};
        let argon2 = Argon2::default();
        argon2
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| AppError::Internal(format!("Password hashing failed: {e}")))
    }

    fn verify_password(&self, password: &str, hash: &str) -> Result<(), AppError> {
        use argon2::{
            Argon2,
            password_hash::{PasswordVerifier, phc::PasswordHash},
        };
        let parsed = PasswordHash::new(hash)
            .map_err(|e| AppError::Internal(format!("Invalid password hash: {e}")))?;
        if parsed.salt.is_none() || parsed.hash.is_none() {
            return Err(AppError::Internal("Incomplete password hash".into()));
        }
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|error| match error {
                argon2::password_hash::Error::PasswordInvalid => AppError::Auth {
                    message: "Invalid email or password".into(),
                    code: AuthErrorCode::InvalidCredentials,
                },
                _ => AppError::Internal(format!("Password verification failed: {error}")),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashes_use_unique_salts_and_verify() {
        let service = AuthService::new();
        let first = service.hash_password("a-test-password").unwrap();
        let second = service.hash_password("a-test-password").unwrap();
        assert_ne!(first, second);
        assert!(service.verify_password("a-test-password", &first).is_ok());
        assert!(service.verify_password("a-test-password", &second).is_ok());
    }

    #[test]
    fn verifies_legacy_argon2_hash_and_rejects_wrong_password() {
        // PHC test vector also supported by argon2 0.5, before this upgrade.
        let hash = "$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc";
        let service = AuthService::new();
        assert!(service.verify_password("password", hash).is_ok());
        assert!(matches!(
            service.verify_password("wrong-password", hash),
            Err(AppError::Auth {
                code: AuthErrorCode::InvalidCredentials,
                ..
            })
        ));
    }

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
