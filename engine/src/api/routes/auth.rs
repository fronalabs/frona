use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use crate::api::cookie::{make_auth_cookie, make_clear_cookie};
use crate::dto::{AuthResponse, LoginRequest, RegisterRequest, UserInfo};
use crate::error::AppError;
use crate::repository::Repository;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use super::super::state::AppState;

const TOKEN_MAX_AGE: u64 = 24 * 3600;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
}

async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<([(axum::http::HeaderName, axum::http::HeaderValue); 1], Json<AuthResponse>), ApiError>
{
    let response = state
        .auth_service
        .register(&state.user_repo, req)
        .await?;
    let cookie = make_auth_cookie(&response.token, TOKEN_MAX_AGE);
    Ok(([(SET_COOKIE, cookie)], Json(response)))
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<([(axum::http::HeaderName, axum::http::HeaderValue); 1], Json<AuthResponse>), ApiError>
{
    let response = state.auth_service.login(&state.user_repo, req).await?;
    let cookie = make_auth_cookie(&response.token, TOKEN_MAX_AGE);
    Ok(([(SET_COOKIE, cookie)], Json(response)))
}

async fn me(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UserInfo>, ApiError> {
    let user = state
        .user_repo
        .find_by_id(&auth.user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    Ok(Json(UserInfo {
        id: user.id,
        email: user.email,
        name: user.name,
    }))
}

async fn logout() -> ([(axum::http::HeaderName, axum::http::HeaderValue); 1], StatusCode) {
    ([(SET_COOKIE, make_clear_cookie())], StatusCode::NO_CONTENT)
}
