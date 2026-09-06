use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use super::super::error::ApiError;
use crate::core::error::{AppError, AuthErrorCode};
use crate::core::principal::{Principal, PrincipalKind};
use crate::core::state::AppState;

/// Accepts any principal kind (User, Agent, McpServer, App).
pub struct AuthPrincipal {
    pub user_id: String,
    pub handle: crate::core::Handle,
    pub email: String,
    pub token_id: String,
    pub token_type: String,
    pub principal: Principal,
    pub scopes: Option<Vec<String>>,
    pub extensions: Option<serde_json::Value>,
}

impl AuthPrincipal {
    pub fn agent_id(&self) -> Option<&str> {
        match self.principal.kind {
            PrincipalKind::Agent => Some(&self.principal.id),
            _ => None,
        }
    }
}

impl FromRequestParts<AppState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_token(parts)?;
        let claims = state
            .token_service
            .validate(&state.keypair_service, token)
            .await?;

        Ok(AuthPrincipal {
            user_id: claims.sub,
            handle: claims.handle,
            email: claims.email,
            token_id: claims.token_id,
            token_type: claims.token_type,
            principal: claims.principal,
            scopes: claims.scopes,
            extensions: claims.extensions,
        })
    }
}

/// Rejects non-User principals.
pub struct AuthUser {
    pub user_id: String,
    pub handle: crate::core::Handle,
    pub email: String,
    pub token_id: String,
    pub token_type: String,
    pub principal: Principal,
    pub scopes: Option<Vec<String>>,
    pub extensions: Option<serde_json::Value>,
}

/// Global administration permission, checked before body extraction or handlers.
pub struct AdminUser(pub AuthUser);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth = AuthUser::from_request_parts(parts, state).await?;
        auth.require_admin(state).await?;
        Ok(Self(auth))
    }
}

impl AuthUser {
    pub async fn require_admin(&self, state: &AppState) -> Result<(), ApiError> {
        let user = state
            .user_service
            .find_by_id(&self.user_id)
            .await?
            .ok_or_else(|| ApiError(AppError::NotFound("User not found".into())))?;
        if !user
            .groups
            .iter()
            .any(|group| group == crate::auth::models::ADMINS_GROUP)
        {
            return Err(ApiError(AppError::Forbidden(
                "Administrator privileges required".into(),
            )));
        }
        Ok(())
    }

    pub fn is_pat(&self) -> bool {
        self.token_type == "pat"
    }

    pub fn is_session(&self) -> bool {
        self.token_type == "access"
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes
            .as_ref()
            .is_some_and(|s| s.iter().any(|sc| sc == scope))
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_token(parts)?;
        let claims = state
            .token_service
            .validate(&state.keypair_service, token)
            .await?;

        if claims.principal.kind != PrincipalKind::User {
            return Err(ApiError(AppError::Forbidden(
                "This endpoint requires a user principal".into(),
            )));
        }

        Ok(AuthUser {
            user_id: claims.sub,
            handle: claims.handle,
            email: claims.email,
            token_id: claims.token_id,
            token_type: claims.token_type,
            principal: claims.principal,
            scopes: claims.scopes,
            extensions: claims.extensions,
        })
    }
}

fn extract_token(parts: &Parts) -> Result<&str, ApiError> {
    if let Some(header) = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        return header.strip_prefix("Bearer ").ok_or_else(|| {
            ApiError(AppError::Auth {
                message: "Invalid authorization format".into(),
                code: AuthErrorCode::InvalidCredentials,
            })
        });
    }

    Err(ApiError(AppError::Auth {
        message: "Missing authorization".into(),
        code: AuthErrorCode::InvalidCredentials,
    }))
}
