use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/browser/debugger/{credential_id}",
        get(debugger_proxy),
    )
}

async fn debugger_proxy(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(credential_id): Path<String>,
) -> Result<Response, ApiError> {
    let credential = state
        .vault_service
        .find_credential_by_id(&credential_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(crate::core::error::AppError::NotFound("Credential not found".into())))?;

    if credential.user_id != auth.user_id {
        return Err(ApiError::from(crate::core::error::AppError::Forbidden(
            "Not your credential".into(),
        )));
    }

    let browser_config = state.browser_session_manager.config().ok_or_else(|| {
        ApiError::from(crate::core::error::AppError::Browser(
            "Browser is not configured".into(),
        ))
    })?;
    let browserless_base = browser_config.http_base_url();

    let profile_path = browser_config.profile_path(&auth.username, &credential.provider);
    let target_url = format!(
        "{}/debugger?--user-data-dir={}",
        browserless_base,
        profile_path.display()
    );

    let client = Client::builder(TokioExecutor::new()).build_http();

    let req = Request::get(&target_url)
        .body(Body::empty())
        .map_err(|e| {
            ApiError::from(crate::core::error::AppError::Browser(format!(
                "Failed to build proxy request: {e}"
            )))
        })?;

    let resp = client.request(req).await.map_err(|e| {
        ApiError::from(crate::core::error::AppError::Browser(format!(
            "Failed to proxy to browserless: {e}"
        )))
    })?;

    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.map_err(|e| {
        ApiError::from(crate::core::error::AppError::Browser(format!(
            "Failed to read proxy response: {e}"
        )))
    })?;

    Ok(Response::from_parts(parts, Body::from(bytes.to_bytes())))
}
