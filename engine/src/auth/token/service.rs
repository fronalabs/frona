use std::sync::Arc;

use chrono::{Duration, Utc};

use super::models::{ApiToken, CreatePatRequest, PatListItem, PatResponse, TokenType};
use super::repository::TokenRepository;
use crate::auth::jwt::JwtService;
use crate::auth::models::Claims;
use crate::credential::keypair::service::KeyPairService;
use crate::core::error::AppError;
use crate::auth::User;

#[derive(Clone)]
pub struct TokenService {
    repo: Arc<dyn TokenRepository>,
    jwt: JwtService,
    access_expiry_secs: u64,
    refresh_expiry_secs: u64,
}

impl TokenService {
    pub fn new(
        repo: Arc<dyn TokenRepository>,
        jwt: JwtService,
        access_expiry_secs: u64,
        refresh_expiry_secs: u64,
    ) -> Self {
        Self {
            repo,
            jwt,
            access_expiry_secs,
            refresh_expiry_secs,
        }
    }

    pub async fn create_session_pair(
        &self,
        keypair_svc: &KeyPairService,
        user: &User,
    ) -> Result<(String, String), AppError> {
        let pair_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();

        let owner = format!("user:{}", user.id);
        let (encoding_key, kid) = keypair_svc.get_signing_key(&owner).await?;

        // Create access token
        let access_id = uuid::Uuid::new_v4().to_string();
        let access_expires = now + Duration::seconds(self.access_expiry_secs as i64);
        let access_claims = Claims {
            sub: user.id.clone(),
            username: user.username.clone(),
            email: user.email.clone(),
            exp: access_expires.timestamp() as usize,
            iat: now.timestamp() as usize,
            token_id: access_id.clone(),
            token_type: "access".to_string(),
            agent_id: None,
            scopes: None,
        };
        let access_jwt = self.jwt.sign(&access_claims, &encoding_key, &kid)?;

        let access_token = ApiToken {
            id: access_id,
            user_id: user.id.clone(),
            name: "session".to_string(),
            token_type: TokenType::Access,
            agent_id: None,
            scopes: vec![],
            prefix: token_prefix(&access_jwt),
            expires_at: access_expires,
            last_used_at: None,
            refresh_pair_id: Some(pair_id.clone()),
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&access_token).await?;

        // Create refresh token
        let refresh_id = uuid::Uuid::new_v4().to_string();
        let refresh_expires = now + Duration::seconds(self.refresh_expiry_secs as i64);
        let refresh_claims = Claims {
            sub: user.id.clone(),
            username: user.username.clone(),
            email: user.email.clone(),
            exp: refresh_expires.timestamp() as usize,
            iat: now.timestamp() as usize,
            token_id: refresh_id.clone(),
            token_type: "refresh".to_string(),
            agent_id: None,
            scopes: None,
        };
        let refresh_jwt = self.jwt.sign(&refresh_claims, &encoding_key, &kid)?;

        let refresh_token = ApiToken {
            id: refresh_id,
            user_id: user.id.clone(),
            name: "refresh".to_string(),
            token_type: TokenType::Refresh,
            agent_id: None,
            scopes: vec![],
            prefix: token_prefix(&refresh_jwt),
            expires_at: refresh_expires,
            last_used_at: None,
            refresh_pair_id: Some(pair_id),
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&refresh_token).await?;

        Ok((access_jwt, refresh_jwt))
    }

    pub async fn create_access_token(
        &self,
        keypair_svc: &KeyPairService,
        user: &User,
        name: &str,
    ) -> Result<String, AppError> {
        let now = Utc::now();

        let owner = format!("user:{}", user.id);
        let (encoding_key, kid) = keypair_svc.get_signing_key(&owner).await?;

        let token_id = uuid::Uuid::new_v4().to_string();
        let expires_at = now + Duration::seconds(self.access_expiry_secs as i64);
        let claims = Claims {
            sub: user.id.clone(),
            username: user.username.clone(),
            email: user.email.clone(),
            exp: expires_at.timestamp() as usize,
            iat: now.timestamp() as usize,
            token_id: token_id.clone(),
            token_type: "access".to_string(),
            agent_id: None,
            scopes: None,
        };
        let jwt = self.jwt.sign(&claims, &encoding_key, &kid)?;

        let api_token = ApiToken {
            id: token_id,
            user_id: user.id.clone(),
            name: name.to_string(),
            token_type: TokenType::Access,
            agent_id: None,
            scopes: vec![],
            prefix: token_prefix(&jwt),
            expires_at,
            last_used_at: None,
            refresh_pair_id: None,
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&api_token).await?;

        Ok(jwt)
    }

    pub async fn refresh(
        &self,
        keypair_svc: &KeyPairService,
        refresh_token_str: &str,
    ) -> Result<(String, String, Claims), AppError> {
        let claims = self.validate(keypair_svc, refresh_token_str).await?;

        if claims.token_type != "refresh" {
            return Err(AppError::Auth("Not a refresh token".into()));
        }

        // Find the old pair and delete it
        if let Some(ref pair_id) = self
            .repo
            .find_active_by_id(&claims.token_id)
            .await?
            .and_then(|t| t.refresh_pair_id)
        {
            self.repo.delete_by_refresh_pair(pair_id).await?;
        }

        // Look up user info from claims to create new pair
        let user = User {
            id: claims.sub.clone(),
            username: claims.username.clone(),
            email: claims.email.clone(),
            name: String::new(),
            password_hash: String::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let (access_jwt, refresh_jwt) = self.create_session_pair(keypair_svc, &user).await?;
        Ok((access_jwt, refresh_jwt, claims))
    }

    pub async fn create_pat(
        &self,
        keypair_svc: &KeyPairService,
        user: &User,
        req: CreatePatRequest,
    ) -> Result<PatResponse, AppError> {
        let now = Utc::now();
        let expires_in_days = req.expires_in_days.unwrap_or(30);
        let expires_at = now + Duration::days(expires_in_days as i64);
        let scopes = req.scopes.unwrap_or_default();

        let owner = format!("user:{}", user.id);
        let (encoding_key, kid) = keypair_svc.get_signing_key(&owner).await?;

        let token_id = uuid::Uuid::new_v4().to_string();
        let claims = Claims {
            sub: user.id.clone(),
            username: user.username.clone(),
            email: user.email.clone(),
            exp: expires_at.timestamp() as usize,
            iat: now.timestamp() as usize,
            token_id: token_id.clone(),
            token_type: "pat".to_string(),
            agent_id: None,
            scopes: Some(scopes.clone()),
        };
        let jwt = self.jwt.sign(&claims, &encoding_key, &kid)?;

        let api_token = ApiToken {
            id: token_id.clone(),
            user_id: user.id.clone(),
            name: req.name.clone(),
            token_type: TokenType::Pat,
            agent_id: None,
            scopes: scopes.clone(),
            prefix: token_prefix(&jwt),
            expires_at,
            last_used_at: None,
            refresh_pair_id: None,
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&api_token).await?;

        Ok(PatResponse {
            id: token_id,
            name: req.name,
            prefix: token_prefix(&jwt),
            token: jwt,
            scopes,
            expires_at,
            created_at: now,
        })
    }

    pub async fn validate(
        &self,
        keypair_svc: &KeyPairService,
        token_str: &str,
    ) -> Result<Claims, AppError> {
        let header = self.jwt.decode_unverified_header(token_str)?;
        let kid = header
            .kid
            .ok_or_else(|| AppError::Auth("Token missing kid".into()))?;

        let decoding_key = keypair_svc.get_verifying_key(&kid).await?;
        let claims = self.jwt.verify::<Claims>(token_str, &decoding_key)?;

        // Check DB for revocation
        let db_token = self
            .repo
            .find_active_by_id(&claims.token_id)
            .await?
            .ok_or_else(|| AppError::Auth("Token revoked".into()))?;

        // Update last_used_at (fire-and-forget for access tokens)
        let _ = self.repo.update_last_used(&db_token.id).await;

        Ok(claims)
    }

    pub async fn revoke_session(&self, pair_id: &str) -> Result<(), AppError> {
        self.repo.delete_by_refresh_pair(pair_id).await
    }

    pub async fn list_pats(&self, user_id: &str) -> Result<Vec<PatListItem>, AppError> {
        let tokens = self.repo.find_by_user_id(user_id).await?;
        Ok(tokens
            .into_iter()
            .filter(|t| t.token_type == TokenType::Pat)
            .map(|t| PatListItem {
                id: t.id,
                name: t.name,
                prefix: t.prefix,
                scopes: t.scopes,
                expires_at: t.expires_at,
                last_used_at: t.last_used_at,
                created_at: t.created_at,
            })
            .collect())
    }

    pub async fn delete_pat(&self, user_id: &str, token_id: &str) -> Result<(), AppError> {
        let token = self
            .repo
            .find_active_by_id(token_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Token not found".into()))?;

        if token.user_id != user_id {
            return Err(AppError::Forbidden("Not your token".into()));
        }
        if token.token_type != TokenType::Pat {
            return Err(AppError::Validation("Can only delete PATs".into()));
        }

        self.repo.delete(token_id).await
    }

    pub async fn cleanup_expired(&self) -> Result<u64, AppError> {
        self.repo.delete_expired().await
    }

    pub fn refresh_expiry_secs(&self) -> u64 {
        self.refresh_expiry_secs
    }

    pub fn repo(&self) -> &dyn TokenRepository {
        &*self.repo
    }
}

fn token_prefix(jwt: &str) -> String {
    let chars: String = jwt.chars().take(12).collect();
    format!("{chars}...")
}
