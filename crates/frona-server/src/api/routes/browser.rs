use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::response::Response;
use axum::routing::get;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;
use tower_http::timeout::TimeoutBody;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

// Only time spent waiting for a requested upstream frame counts, not downstream backpressure.
const DEBUGGER_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn router() -> Router<AppState> {
    Router::new().route("/api/browser/debugger/{credential_id}", get(debugger_proxy))
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
        .ok_or_else(|| {
            ApiError::from(crate::core::error::AppError::NotFound(
                "Credential not found".into(),
            ))
        })?;

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
    // Request the directory directly to avoid redirects that could expose query tokens.
    let target_url = browser_config
        .http_url(
            &["debugger", ""],
            browser_config.api_token.as_deref(),
            &[(
                "--user-data-dir",
                &browser_config
                    .profile_path(&auth.handle, &credential.provider)
                    .to_string_lossy(),
            )],
        )
        .map_err(|_| {
            crate::core::error::AppError::Browser("Invalid Browserless debugger URL".into())
        })?;

    let client = Client::builder(TokioExecutor::new()).build_http();

    let req = Request::get(target_url.as_str())
        .body(Body::empty())
        .map_err(|e| {
            ApiError::from(crate::core::error::AppError::Browser(format!(
                "Failed to build proxy request: {e}"
            )))
        })?;

    let resp = tokio::time::timeout(
        Duration::from_millis(browser_config.connection_timeout_ms),
        client.request(req),
    )
    .await
    .map_err(|_| crate::core::error::AppError::Http {
        status: 504,
        message: "Browserless debugger response timed out".into(),
    })?
    .map_err(|_| crate::core::error::AppError::Browser("Failed to proxy to Browserless".into()))?;

    if !resp.status().is_success() {
        // Upstream error bodies and redirect headers can echo the authenticated URL.
        return Err(crate::core::error::AppError::Browser(format!(
            "Browserless debugger returned HTTP {}",
            resp.status()
        ))
        .into());
    }

    let (parts, body) = resp.into_parts();
    Ok(Response::from_parts(
        parts,
        Body::new(TimeoutBody::new(DEBUGGER_BODY_IDLE_TIMEOUT, body)),
    ))
}
