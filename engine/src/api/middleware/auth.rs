use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use super::super::error::ApiError;
use super::super::state::AppState;
use crate::api::cookie::extract_token_from_cookie_header;

pub struct AuthUser {
    pub user_id: String,
    pub email: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_token(parts)?;
        let claims = state.auth_service.validate_token(token)?;

        Ok(AuthUser {
            user_id: claims.sub,
            email: claims.email,
        })
    }
}

fn extract_token(parts: &Parts) -> Result<&str, ApiError> {
    if let Some(token) = parts
        .headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(extract_token_from_cookie_header)
    {
        return Ok(token);
    }

    if let Some(header) = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        return header.strip_prefix("Bearer ").ok_or_else(|| {
            ApiError(crate::error::AppError::Auth(
                "Invalid authorization format".into(),
            ))
        });
    }

    if let Some(token) = parts
        .uri
        .query()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("token=")))
    {
        return Ok(token);
    }

    Err(ApiError(crate::error::AppError::Auth(
        "Missing authorization".into(),
    )))
}
