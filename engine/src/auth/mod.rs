pub mod jwt;

use async_trait::async_trait;

use crate::dto::{AuthResponse, LoginRequest, RegisterRequest, UserInfo};
use crate::error::AppError;
use crate::models::User;
use crate::repository::Repository;

use self::jwt::JwtService;

#[async_trait]
pub trait UserRepository: Repository<User> {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, AppError>;
}

pub struct AuthService {
    jwt: JwtService,
}

impl AuthService {
    pub fn new(jwt_secret: &str) -> Self {
        Self {
            jwt: JwtService::new(jwt_secret),
        }
    }

    pub async fn register(
        &self,
        repo: &dyn UserRepository,
        req: RegisterRequest,
    ) -> Result<AuthResponse, AppError> {
        if let Some(_) = repo.find_by_email(&req.email).await? {
            return Err(AppError::Validation("Email already registered".into()));
        }

        let password_hash = self.hash_password(&req.password)?;
        let now = chrono::Utc::now();
        let user = User {
            id: uuid::Uuid::new_v4().to_string(),
            email: req.email,
            name: req.name,
            password_hash,
            created_at: now,
            updated_at: now,
        };

        let user = repo.create(&user).await?;
        let token = self.jwt.generate(&user)?;

        Ok(AuthResponse {
            token,
            user: UserInfo {
                id: user.id,
                email: user.email,
                name: user.name,
            },
        })
    }

    pub async fn login(
        &self,
        repo: &dyn UserRepository,
        req: LoginRequest,
    ) -> Result<AuthResponse, AppError> {
        let user = repo
            .find_by_email(&req.email)
            .await?
            .ok_or_else(|| AppError::Auth("Invalid email or password".into()))?;

        self.verify_password(&req.password, &user.password_hash)?;
        let token = self.jwt.generate(&user)?;

        Ok(AuthResponse {
            token,
            user: UserInfo {
                id: user.id,
                email: user.email,
                name: user.name,
            },
        })
    }

    pub fn validate_token(&self, token: &str) -> Result<crate::dto::Claims, AppError> {
        self.jwt.validate(token)
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
