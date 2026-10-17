use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use super::models::{ApiToken, CreatePatRequest, PatListItem, PatResponse, TokenType};
use super::repository::TokenRepository;
use crate::auth::User;
use crate::auth::jwt::JwtService;
use crate::auth::models::Claims;
use crate::core::Principal;
use crate::core::error::{AppError, AuthErrorCode};
use crate::credential::keypair::service::KeyPairService;

#[derive(Clone)]
pub struct TokenService {
    repo: Arc<dyn TokenRepository>,
    jwt: JwtService,
    user_service: super::super::user_service::UserService,
    access_expiry_secs: u64,
    refresh_expiry_secs: u64,
}

pub struct CreateTokenRequest {
    pub token_type: TokenType,
    pub principal: Principal,
    pub ttl_secs: u64,
    pub name: String,
    pub scopes: Vec<String>,
    pub refresh_pair_id: Option<String>,
    pub extensions: Option<serde_json::Value>,
}

pub struct CreatedToken {
    pub jwt: String,
    pub token_id: String,
    pub expires_at: DateTime<Utc>,
}

impl TokenService {
    pub fn new(
        repo: Arc<dyn TokenRepository>,
        jwt: JwtService,
        user_service: super::super::user_service::UserService,
        access_expiry_secs: u64,
        refresh_expiry_secs: u64,
    ) -> Self {
        Self {
            repo,
            jwt,
            user_service,
            access_expiry_secs,
            refresh_expiry_secs,
        }
    }

    pub async fn create_token(
        &self,
        keypair_svc: &KeyPairService,
        user: &User,
        req: CreateTokenRequest,
    ) -> Result<CreatedToken, AppError> {
        let now = Utc::now();
        let expires_at = now + Duration::seconds(req.ttl_secs as i64);
        let token_id = crate::core::repository::new_id();

        let owner = format!("user:{}", user.id);
        let (encoding_key, kid) = keypair_svc.get_signing_key(&owner).await?;

        let scopes_claim = if req.scopes.is_empty() {
            None
        } else {
            Some(req.scopes.clone())
        };

        let claims = Claims {
            sub: user.id.clone(),
            handle: user.handle.clone(),
            email: user.email.clone(),
            exp: expires_at.timestamp() as usize,
            iat: now.timestamp() as usize,
            token_id: token_id.clone(),
            token_type: req.token_type.as_str().to_string(),
            principal: req.principal.clone(),
            scopes: scopes_claim,
            extensions: req.extensions,
        };

        let jwt = self.jwt.sign(&claims, &encoding_key, &kid)?;

        if !req.token_type.is_stateless() {
            let api_token = ApiToken {
                id: token_id.clone(),
                user_id: user.id.clone(),
                name: req.name,
                token_type: req.token_type,
                principal: req.principal,
                scopes: req.scopes,
                prefix: token_prefix(&jwt),
                expires_at,
                last_used_at: None,
                refresh_pair_id: req.refresh_pair_id,
                created_at: now,
                updated_at: now,
            };
            self.repo.create(&api_token).await?;
        }

        Ok(CreatedToken {
            jwt,
            token_id,
            expires_at,
        })
    }

    pub async fn create_session_pair(
        &self,
        keypair_svc: &KeyPairService,
        user: &User,
    ) -> Result<(String, String), AppError> {
        let pair_id = crate::core::repository::new_id();
        let principal = Principal::user(&user.id);

        let access = self
            .create_token(
                keypair_svc,
                user,
                CreateTokenRequest {
                    token_type: TokenType::Access,
                    principal: principal.clone(),
                    ttl_secs: self.access_expiry_secs,
                    name: "session".to_string(),
                    scopes: vec![],
                    refresh_pair_id: Some(pair_id.clone()),
                    extensions: None,
                },
            )
            .await?;

        let refresh = self
            .create_token(
                keypair_svc,
                user,
                CreateTokenRequest {
                    token_type: TokenType::Refresh,
                    principal,
                    ttl_secs: self.refresh_expiry_secs,
                    name: "refresh".to_string(),
                    scopes: vec![],
                    refresh_pair_id: Some(pair_id),
                    extensions: None,
                },
            )
            .await?;

        Ok((access.jwt, refresh.jwt))
    }

    pub async fn create_access_token(
        &self,
        keypair_svc: &KeyPairService,
        user: &User,
        name: &str,
    ) -> Result<String, AppError> {
        let created = self
            .create_token(
                keypair_svc,
                user,
                CreateTokenRequest {
                    token_type: TokenType::Access,
                    principal: Principal::user(&user.id),
                    ttl_secs: self.access_expiry_secs,
                    name: name.to_string(),
                    scopes: vec![],
                    refresh_pair_id: None,
                    extensions: None,
                },
            )
            .await?;
        Ok(created.jwt)
    }

    pub async fn refresh(
        &self,
        keypair_svc: &KeyPairService,
        refresh_token_str: &str,
    ) -> Result<(String, String, Claims), AppError> {
        let claims = self
            .validate_refresh(keypair_svc, refresh_token_str)
            .await?;

        let pair_id = self
            .repo
            .find_active_by_id(&claims.token_id)
            .await?
            .and_then(|token| token.refresh_pair_id)
            .ok_or_else(|| AppError::Auth {
                message: "Refresh token already consumed or missing its session pair".into(),
                code: AuthErrorCode::TokenInvalid,
            })?;

        if !self
            .repo
            .consume_refresh_pair(&claims.token_id, &pair_id)
            .await?
        {
            return Err(AppError::Auth {
                message: "Refresh token already consumed".into(),
                code: AuthErrorCode::TokenInvalid,
            });
        }

        let user = self
            .user_service
            .find_by_id(&claims.sub)
            .await?
            .ok_or_else(|| AppError::Auth {
                message: "User no longer exists".into(),
                code: AuthErrorCode::TokenInvalid,
            })?;

        if user.deactivated_at.is_some() {
            return Err(AppError::Auth {
                message: "Account deactivated".into(),
                code: AuthErrorCode::AccountDeactivated,
            });
        }

        let (access_jwt, refresh_jwt) = self.create_session_pair(keypair_svc, &user).await?;
        Ok((access_jwt, refresh_jwt, claims))
    }

    pub async fn create_pat(
        &self,
        keypair_svc: &KeyPairService,
        user: &User,
        req: CreatePatRequest,
    ) -> Result<PatResponse, AppError> {
        let expires_in_days = req.expires_in_days.unwrap_or(30);
        let ttl_secs = expires_in_days.saturating_mul(86_400);
        let scopes = req.scopes.unwrap_or_default();
        let principal = req.principal.unwrap_or_else(|| Principal::user(&user.id));

        let created = self
            .create_token(
                keypair_svc,
                user,
                CreateTokenRequest {
                    token_type: TokenType::Pat,
                    principal,
                    ttl_secs,
                    name: req.name.clone(),
                    scopes: scopes.clone(),
                    refresh_pair_id: None,
                    extensions: None,
                },
            )
            .await?;

        Ok(PatResponse {
            id: created.token_id,
            name: req.name,
            prefix: token_prefix(&created.jwt),
            token: created.jwt,
            scopes,
            expires_at: created.expires_at,
            created_at: Utc::now(),
        })
    }

    /// Ordinary API and app bearer credentials. Principal and scope checks remain
    /// the caller's responsibility after authentication.
    pub async fn validate_api(
        &self,
        keypair_svc: &KeyPairService,
        token_str: &str,
    ) -> Result<Claims, AppError> {
        self.validate_types(
            keypair_svc,
            token_str,
            &[TokenType::Access, TokenType::Pat, TokenType::Ephemeral],
        )
        .await
    }

    /// Refresh cookies may renew sessions or bootstrap an app session, but may
    /// never authenticate an ordinary API request.
    pub async fn validate_refresh(
        &self,
        keypair_svc: &KeyPairService,
        token_str: &str,
    ) -> Result<Claims, AppError> {
        self.validate_types(keypair_svc, token_str, &[TokenType::Refresh])
            .await
    }

    /// App session cookies and voice credentials are issued as access tokens.
    pub async fn validate_access(
        &self,
        keypair_svc: &KeyPairService,
        token_str: &str,
    ) -> Result<Claims, AppError> {
        self.validate_types(keypair_svc, token_str, &[TokenType::Access])
            .await
    }

    async fn validate_types(
        &self,
        keypair_svc: &KeyPairService,
        token_str: &str,
        accepted_types: &[TokenType],
    ) -> Result<Claims, AppError> {
        let header = self.jwt.decode_unverified_header(token_str)?;
        let kid = header.kid.ok_or_else(|| AppError::Auth {
            message: "Token missing kid".into(),
            code: AuthErrorCode::TokenInvalid,
        })?;

        let decoding_key = keypair_svc.get_verifying_key(&kid).await?;
        let claims = self.jwt.verify::<Claims>(token_str, &decoding_key)?;

        // Check the signed type before the stateless shortcut or DB lookup.
        // Unknown future token types must opt in at each consuming entry point.
        if !accepted_types
            .iter()
            .any(|token_type| claims.token_type == token_type.as_str())
        {
            return Err(AppError::Auth {
                message: "Token type is not accepted by this endpoint".into(),
                code: AuthErrorCode::TokenInvalid,
            });
        }

        // Stateless tokens (Ephemeral) skip DB revocation checks — signature + exp are authoritative.
        if claims.token_type == TokenType::Ephemeral.as_str() {
            return Ok(claims);
        }

        let db_token = self
            .repo
            .find_active_by_id(&claims.token_id)
            .await?
            .ok_or_else(|| AppError::Auth {
                message: "Token revoked".into(),
                code: AuthErrorCode::TokenInvalid,
            })?;

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
