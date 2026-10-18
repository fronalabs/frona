use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::SmartIpKeyExtractor;

use crate::api::cookie::{
    extract_refresh_token_from_cookie_header, extract_sso_csrf_from_cookie_header,
    make_clear_refresh_cookie, make_clear_sso_csrf_cookie, make_refresh_cookie,
    make_sso_csrf_cookie,
};
use crate::auth::models::{
    AuthResponse, LoginRequest, RegisterRequest, UpdateHandleRequest, UpdateProfileRequest,
    UserInfo,
};
use crate::auth::token::models::CreatePatRequest;
use crate::core::error::{AppError, AuthErrorCode};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    let auth_limit = GovernorConfigBuilder::default()
        .per_second(2)
        .burst_size(5)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .unwrap();

    let refresh_limit = GovernorConfigBuilder::default()
        .per_second(2)
        .burst_size(5)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .unwrap();

    let rate_limited_auth = Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/register", post(register))
        .layer(GovernorLayer::new(auth_limit));

    let rate_limited_refresh = Router::new()
        .route("/api/auth/refresh", post(refresh))
        .layer(GovernorLayer::new(refresh_limit));

    Router::new()
        .merge(rate_limited_auth)
        .merge(rate_limited_refresh)
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/handle", put(change_handle))
        .route("/api/auth/profile", put(update_profile))
        .route("/api/auth/tokens", post(create_pat).get(list_pats))
        .route("/api/auth/tokens/{id}", delete(delete_pat))
        .route("/api/auth/config", get(auth_config))
        .route("/api/auth/sso/authorize", get(sso_authorize))
        .route("/api/auth/sso/callback", get(sso_callback))
}

async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<
    (
        StatusCode,
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        Json<AuthResponse>,
    ),
    ApiError,
> {
    if state.config.sso.disable_local_auth {
        return Err(ApiError(AppError::Validation(
            "SSO registration required".into(),
        )));
    }
    if !state.config.auth.allow_registration {
        return Err(ApiError(AppError::Forbidden(
            "Registration is disabled".into(),
        )));
    }

    let (response, refresh_jwt) = state
        .auth_service
        .register(
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
            &state.policy_service,
            req,
        )
        .await?;
    state
        .agent_service
        .clone_all_builtins_for_user(&response.user.id, &state.storage_service)
        .await?;

    let secure = state
        .config
        .server
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    Ok((StatusCode::CREATED, [(SET_COOKIE, cookie)], Json(response)))
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<
    (
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        Json<AuthResponse>,
    ),
    ApiError,
> {
    if state.config.sso.disable_local_auth {
        return Err(ApiError(AppError::Validation("SSO login required".into())));
    }

    let identifier = req.identifier.clone();

    if state.login_tracker.is_locked(&identifier).await {
        return Err(ApiError(AppError::Auth {
            message: "Too many failed attempts. Please try again later.".into(),
            code: AuthErrorCode::InvalidCredentials,
        }));
    }

    let result = state
        .auth_service
        .login(
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
            &state.policy_service,
            req,
        )
        .await;

    let (response, refresh_jwt) = match result {
        Ok(v) => {
            state.login_tracker.clear(&identifier).await;
            v
        }
        Err(e) => {
            if matches!(
                e,
                AppError::Auth {
                    code: AuthErrorCode::InvalidCredentials,
                    ..
                }
            ) {
                state.login_tracker.record_failure(&identifier).await;
            }
            return Err(ApiError(e));
        }
    };

    let secure = state
        .config
        .server
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    Ok(([(SET_COOKIE, cookie)], Json(response)))
}

async fn me(auth: AuthUser, State(state): State<AppState>) -> Result<Json<UserInfo>, ApiError> {
    let user = state
        .user_service
        .find_by_id(&auth.user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let setup_completed = state.get_runtime_config_bool("setup_completed").await;
    let needs_setup = if setup_completed { None } else { Some(true) };

    let info = crate::auth::build_user_info(user, &state.policy_service, needs_setup).await?;
    Ok(Json(info))
}

async fn change_handle(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateHandleRequest>,
) -> Result<
    (
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        Json<AuthResponse>,
    ),
    ApiError,
> {
    let (response, refresh_jwt) = state
        .auth_service
        .change_handle(
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
            &state.policy_service,
            &state.storage_service,
            &state.config,
            &auth.user_id,
            req,
        )
        .await?;

    let secure = state
        .config
        .server
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    let cookie = make_refresh_cookie(
        &refresh_jwt,
        state.token_service.refresh_expiry_secs(),
        secure,
    );

    Ok(([(SET_COOKIE, cookie)], Json(response)))
}

async fn update_profile(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<Json<UserInfo>, ApiError> {
    let user_info = state
        .auth_service
        .update_profile(
            &state.user_service,
            &state.policy_service,
            &auth.user_id,
            req,
        )
        .await?;
    Ok(Json(user_info))
}

async fn logout(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<
    (
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        StatusCode,
    ),
    ApiError,
> {
    if let Some(token) = state
        .token_service
        .repo()
        .find_active_by_id(&auth.token_id)
        .await?
    {
        if let Some(pair_id) = &token.refresh_pair_id {
            state.token_service.revoke_session(pair_id).await?;
        } else {
            state.token_service.repo().delete(&auth.token_id).await?;
        }
    }

    let secure = state
        .config
        .server
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    Ok((
        [(SET_COOKIE, make_clear_refresh_cookie(secure))],
        StatusCode::NO_CONTENT,
    ))
}

async fn refresh(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<
    (
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        Json<serde_json::Value>,
    ),
    ApiError,
> {
    let refresh_token = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(extract_refresh_token_from_cookie_header)
        .ok_or_else(|| AppError::Auth {
            message: "Missing refresh token".into(),
            code: AuthErrorCode::TokenInvalid,
        })?;

    // A competing tab may already have rotated this cookie. On failure, leave
    // cookies alone so a delayed response cannot erase the winning replacement.
    // The losing tab can retry with the shared cookie; cross-tab retry coordination
    // is a separate client concern.
    let (access_jwt, new_refresh_jwt, _claims) = state
        .token_service
        .refresh(&state.keypair_service, refresh_token)
        .await?;

    let secure = state
        .config
        .server
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
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

    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(pat).unwrap()),
    ))
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
    state.token_service.delete_pat(&auth.user_id, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Serialize)]
struct SsoStatus {
    enabled: bool,
    disable_local_auth: bool,
}

#[derive(serde::Serialize)]
struct AuthConfigResponse {
    sso: SsoStatus,
    allow_registration: bool,
}

async fn auth_config(State(state): State<AppState>) -> Json<AuthConfigResponse> {
    Json(AuthConfigResponse {
        sso: SsoStatus {
            enabled: state.config.sso.enabled,
            disable_local_auth: state.config.sso.disable_local_auth,
        },
        allow_registration: state.config.auth.allow_registration,
    })
}

async fn sso_authorize(
    State(state): State<AppState>,
) -> Result<
    (
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        axum::response::Redirect,
    ),
    ApiError,
> {
    let oauth_svc = state
        .oauth_service
        .as_ref()
        .ok_or_else(|| AppError::Validation("SSO is not enabled".into()))?;

    let (auth_url, csrf_secret, _nonce) = oauth_svc.get_authorization_url().await?;
    let secure = state
        .config
        .server
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    let cookie = make_sso_csrf_cookie(&csrf_secret, secure);
    Ok((
        [(SET_COOKIE, cookie)],
        axum::response::Redirect::temporary(&auth_url),
    ))
}

async fn sso_callback(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let secure = state
        .config
        .server
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));

    match sso_callback_inner(&state, &headers, &params).await {
        Ok(refresh_jwt) => {
            let refresh_cookie = make_refresh_cookie(
                &refresh_jwt,
                state.token_service.refresh_expiry_secs(),
                secure,
            );
            let clear_csrf = make_clear_sso_csrf_cookie(secure);

            axum::response::IntoResponse::into_response((
                axum::response::AppendHeaders([
                    (SET_COOKIE, refresh_cookie),
                    (SET_COOKIE, clear_csrf),
                ]),
                axum::response::Redirect::temporary("/auth/sso/callback"),
            ))
        }
        Err(e) => {
            tracing::warn!(error = %e, "SSO callback failed");
            let clear_csrf = make_clear_sso_csrf_cookie(secure);
            let code = match &e {
                AppError::Auth { code, .. } => code.as_str(),
                _ => AuthErrorCode::ServerError.as_str(),
            };
            let redirect_url = format!("/login?sso_error={code}");

            axum::response::IntoResponse::into_response((
                axum::response::AppendHeaders([(SET_COOKIE, clear_csrf)]),
                axum::response::Redirect::temporary(&redirect_url),
            ))
        }
    }
}

async fn sso_callback_inner(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    params: &std::collections::HashMap<String, String>,
) -> Result<String, AppError> {
    let oauth_svc = state
        .oauth_service
        .as_ref()
        .ok_or_else(|| AppError::Validation("SSO is not enabled".into()))?;

    let callback_state = params
        .get("state")
        .ok_or_else(|| AppError::Validation("Missing state parameter".into()))?;

    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let csrf_cookie =
        extract_sso_csrf_from_cookie_header(cookie_header).ok_or_else(|| AppError::Auth {
            message: "Missing SSO CSRF cookie — please restart the login flow".into(),
            code: AuthErrorCode::CsrfFailed,
        })?;
    if csrf_cookie != callback_state {
        return Err(AppError::Auth {
            message: "SSO state mismatch".into(),
            code: AuthErrorCode::CsrfFailed,
        });
    }

    let code = params
        .get("code")
        .ok_or_else(|| AppError::Validation("Missing authorization code".into()))?;

    let (user, is_new) = oauth_svc
        .handle_callback(
            code,
            callback_state,
            &state.user_service,
            &state.keypair_service,
            &state.token_service,
        )
        .await?;

    if is_new {
        state
            .agent_service
            .clone_all_builtins_for_user(&user.id, &state.storage_service)
            .await?;
    }

    let (_access_jwt, refresh_jwt) = state
        .token_service
        .create_session_pair(&state.keypair_service, &user)
        .await?;

    Ok(refresh_jwt)
}
