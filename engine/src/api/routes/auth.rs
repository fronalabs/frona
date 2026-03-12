use axum::extract::{Path, State};
use axum::http::header::SET_COOKIE;
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::SmartIpKeyExtractor;

use crate::api::cookie::{
    extract_refresh_token_from_cookie_header, make_clear_refresh_cookie, make_refresh_cookie,
};
use crate::auth::models::{AuthResponse, LoginRequest, RegisterRequest, UpdateUsernameRequest, UserInfo};
use crate::auth::token::models::CreatePatRequest;
use crate::core::error::AppError;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(5)
        .burst_size(10)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .unwrap();

    let rate_limited = Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/register", post(register))
        .layer(GovernorLayer::new(governor_conf));

    Router::new()
        .merge(rate_limited)
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/refresh", post(refresh))
        .route("/api/auth/username", put(change_username))
        .route("/api/auth/tokens", post(create_pat).get(list_pats))
        .route("/api/auth/tokens/{id}", delete(delete_pat))
        .route("/api/auth/sso", get(sso_status))
        .route("/api/auth/sso/authorize", get(sso_authorize))
        .route("/api/auth/sso/callback", get(sso_callback))
}

async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<(StatusCode, [(axum::http::HeaderName, axum::http::HeaderValue); 1], Json<AuthResponse>), ApiError>
{
    if state.config.sso.only {
        return Err(ApiError(AppError::Validation(
            "SSO registration required".into(),
        )));
    }

    let (response, refresh_jwt) = state
        .auth_service
        .register(
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
            req,
        )
        .await?;

    let secure = state.config.server.base_url.as_deref().is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    Ok((
        StatusCode::CREATED,
        [(SET_COOKIE, cookie)],
        Json(response),
    ))
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<([(axum::http::HeaderName, axum::http::HeaderValue); 1], Json<AuthResponse>), ApiError>
{
    if state.config.sso.only {
        return Err(ApiError(AppError::Validation(
            "SSO login required".into(),
        )));
    }

    let (response, refresh_jwt) = state
        .auth_service
        .login(
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
            req,
        )
        .await?;

    let secure = state.config.server.base_url.as_deref().is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    Ok(([(SET_COOKIE, cookie)], Json(response)))
}

async fn me(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UserInfo>, ApiError> {
    let user = state
        .user_service
        .find_by_id(&auth.user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let setup_completed = state.get_runtime_config_bool("setup_completed").await;
    let needs_setup = if setup_completed { None } else { Some(true) };

    Ok(Json(UserInfo {
        id: user.id,
        username: user.username,
        email: user.email,
        name: user.name,
        needs_setup,
    }))
}

async fn change_username(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateUsernameRequest>,
) -> Result<([(axum::http::HeaderName, axum::http::HeaderValue); 1], Json<AuthResponse>), ApiError>
{
    let (response, refresh_jwt) = state
        .auth_service
        .change_username(
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
            &state.config,
            &auth.user_id,
            req,
        )
        .await?;

    let secure = state.config.server.base_url.as_deref().is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    Ok(([(SET_COOKIE, cookie)], Json(response)))
}

async fn logout(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<([(axum::http::HeaderName, axum::http::HeaderValue); 1], StatusCode), ApiError> {
    // Find the token's pair_id and revoke the session
    if let Some(token) = state
        .token_service
        .repo()
        .find_active_by_id(&auth.token_id)
        .await?
    {
        if let Some(pair_id) = &token.refresh_pair_id {
            state.token_service.revoke_session(pair_id).await?;
        } else {
            // Single token (PAT), just delete it
            state.token_service.repo().delete(&auth.token_id).await?;
        }
    }

    let secure = state.config.server.base_url.as_deref().is_some_and(|u| u.starts_with("https://"));
    Ok((
        [(SET_COOKIE, make_clear_refresh_cookie(secure))],
        StatusCode::NO_CONTENT,
    ))
}

async fn refresh(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<([(axum::http::HeaderName, axum::http::HeaderValue); 1], Json<serde_json::Value>), ApiError>
{
    let refresh_token = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(extract_refresh_token_from_cookie_header)
        .ok_or_else(|| AppError::Auth("Missing refresh token".into()))?;

    let (access_jwt, new_refresh_jwt, _claims) = state
        .token_service
        .refresh(&state.keypair_service, refresh_token)
        .await?;

    let secure = state.config.server.base_url.as_deref().is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &new_refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({ "token": access_jwt })),
    ))
}

async fn create_pat(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreatePatRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if auth.is_pat() {
        return Err(ApiError(AppError::Forbidden(
            "PATs cannot create other tokens".into(),
        )));
    }

    let user = state
        .user_service
        .find_by_id(&auth.user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let pat = state
        .token_service
        .create_pat(&state.keypair_service, &user, req)
        .await?;

    Ok((StatusCode::CREATED, Json(serde_json::to_value(pat).unwrap())))
}

async fn list_pats(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pats = state.token_service.list_pats(&auth.user_id).await?;
    Ok(Json(serde_json::to_value(pats).unwrap()))
}

async fn delete_pat(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .token_service
        .delete_pat(&auth.user_id, &id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Serialize)]
struct SsoStatusResponse {
    enabled: bool,
    sso_only: bool,
}

async fn sso_status(
    State(state): State<AppState>,
) -> Json<SsoStatusResponse> {
    Json(SsoStatusResponse {
        enabled: state.config.sso.enabled,
        sso_only: state.config.sso.only,
    })
}

async fn sso_authorize(
    State(state): State<AppState>,
) -> Result<axum::response::Redirect, ApiError> {
    let oauth_svc = state
        .oauth_service
        .as_ref()
        .ok_or_else(|| AppError::Validation("SSO is not enabled".into()))?;

    let (auth_url, _csrf, _nonce) = oauth_svc.get_authorization_url().await?;
    Ok(axum::response::Redirect::temporary(&auth_url))
}

async fn sso_callback(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<([(axum::http::HeaderName, axum::http::HeaderValue); 1], axum::response::Redirect), ApiError>
{
    let oauth_svc = state
        .oauth_service
        .as_ref()
        .ok_or_else(|| AppError::Validation("SSO is not enabled".into()))?;

    let code = params
        .get("code")
        .ok_or_else(|| AppError::Validation("Missing authorization code".into()))?;
    let callback_state = params
        .get("state")
        .ok_or_else(|| AppError::Validation("Missing state parameter".into()))?;

    let (user, _is_new) = oauth_svc
        .handle_callback(
            code,
            callback_state,
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
        )
        .await?;

    let (_access_jwt, refresh_jwt) = state
        .token_service
        .create_session_pair(&state.keypair_service, &user)
        .await?;

    let secure = state.config.server.base_url.as_deref().is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    // Redirect to frontend callback page — the frontend will fetch a new access token via refresh
    Ok((
        [(SET_COOKIE, cookie)],
        axum::response::Redirect::temporary("/auth/sso/callback"),
    ))
}
