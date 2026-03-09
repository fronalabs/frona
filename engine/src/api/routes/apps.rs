use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower::ServiceExt as _;
use tower_http::services::ServeDir;

use crate::api::cookie::{
    extract_app_session_from_cookie_header, make_app_session_cookie,
};
use crate::app::models::{App, AppResponse, AppStatus};
use crate::core::state::AppState;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/apps", get(list_apps))
        .route("/api/apps/{id}", get(get_app).delete(delete_app))
        .route("/api/apps/{id}/stop", post(stop_app))
        .route("/api/apps/{id}/restart", post(restart_app))
        .route("/api/apps/approve", post(approve_service))
        .route("/api/apps/deny", post(deny_service))
        .route("/api/auth/apps", get(auth_gate))
        .route("/apps/{id}", any(proxy_app_root))
        .route("/apps/{id}/", any(proxy_app_root))
        .route("/apps/{id}/{*path}", any(proxy_app_path))
}

async fn get_user_app(state: &AppState, auth: &AuthUser, id: &str) -> Result<App, ApiError> {
    let app = state
        .app_service
        .get(id)
        .await?
        .ok_or_else(|| ApiError::from(crate::core::error::AppError::NotFound("App not found".into())))?;
    if app.user_id != auth.user_id {
        return Err(ApiError::from(crate::core::error::AppError::Forbidden(
            "Not your app".into(),
        )));
    }
    Ok(app)
}

async fn list_apps(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<AppResponse>>, ApiError> {
    let apps = state.app_service.list_by_user(&auth.user_id).await?;
    Ok(Json(apps))
}

async fn get_app(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AppResponse>, ApiError> {
    let app = state.app_service.get_by_user(&auth.user_id, &id).await?;
    Ok(Json(app))
}

async fn delete_app(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(), ApiError> {
    let app = get_user_app(&state, &auth, &id).await?;
    state.app_service.destroy(&app.agent_id, &id).await?;
    Ok(())
}

async fn stop_app(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AppResponse>, ApiError> {
    let app = get_user_app(&state, &auth, &id).await?;
    let resp = state.app_service.stop(&app.agent_id, &id).await?;
    Ok(Json(resp))
}

async fn restart_app(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AppResponse>, ApiError> {
    let app = get_user_app(&state, &auth, &id).await?;
    let resp = state.app_service.restart(&app.agent_id, &id).await?;
    Ok(Json(resp))
}

#[derive(Deserialize)]
struct ServiceActionRequest {
    chat_id: String,
}

async fn approve_service(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ServiceActionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let chat = state
        .chat_service
        .get_chat(&auth.user_id, &req.chat_id)
        .await
        .map_err(ApiError::from)?;

    let stored_messages = state.chat_service.get_stored_messages(&req.chat_id).await;

    let pending_msg = stored_messages.iter().rev().find(|m| {
        matches!(
            &m.tool,
            Some(crate::chat::message::models::MessageTool::ServiceApproval {
                status: crate::chat::message::models::ToolStatus::Pending,
                ..
            })
        )
    });

    let Some(pending_msg) = pending_msg else {
        return Err(ApiError::from(crate::core::error::AppError::NotFound(
            "No pending service approval found".into(),
        )));
    };

    let manifest_value = match &pending_msg.tool {
        Some(crate::chat::message::models::MessageTool::ServiceApproval {
            manifest,
            ..
        }) => manifest.clone(),
        _ => unreachable!(),
    };

    let manifest: crate::app::models::AppManifest =
        serde_json::from_value(manifest_value).map_err(|e| {
            ApiError::from(crate::core::error::AppError::Validation(format!(
                "Invalid manifest: {e}"
            )))
        })?;

    let base_url = state.config.server.public_base_url();

    let app = state
        .app_service
        .deploy(&chat.agent_id, &auth.user_id, &manifest, Vec::new())
        .await
        .map_err(ApiError::from)?;

    let url_info = app
        .url
        .as_ref()
        .map(|u| format!("\nURL: {base_url}{u}"))
        .unwrap_or_default();

    let result_text = format!(
        "App '{}' deployed successfully. Status: {}{url_info}",
        app.name, app.status
    );

    let pending_msg_id = pending_msg.id.clone();

    let resolved = state
        .chat_service
        .resolve_tool_message(&pending_msg_id, Some(result_text))
        .await
        .map_err(ApiError::from)?;

    state.broadcast_service.broadcast_chat_message(
        &auth.user_id,
        &req.chat_id,
        resolved,
    );

    let user_id = auth.user_id.clone();
    let chat_id = req.chat_id.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) =
            crate::api::routes::messages::resume_tool_loop(&state_clone, &user_id, &chat_id).await
        {
            tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume after service approval");
        }
    });

    Ok(Json(serde_json::json!({ "approved": true })))
}

async fn deny_service(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ServiceActionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .chat_service
        .get_chat(&auth.user_id, &req.chat_id)
        .await
        .map_err(ApiError::from)?;

    let stored_messages = state.chat_service.get_stored_messages(&req.chat_id).await;

    if let Some(pending_msg) = stored_messages.iter().rev().find(|m| {
        matches!(
            &m.tool,
            Some(crate::chat::message::models::MessageTool::ServiceApproval {
                status: crate::chat::message::models::ToolStatus::Pending,
                ..
            })
        )
    }) {
        let denied = state
            .chat_service
            .deny_tool_message(
                &pending_msg.id,
                Some("User denied the service deployment.".to_string()),
            )
            .await
            .map_err(ApiError::from)?;

        state.broadcast_service.broadcast_chat_message(
            &auth.user_id,
            &req.chat_id,
            denied,
        );
    }

    let user_id = auth.user_id.clone();
    let chat_id = req.chat_id.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) =
            crate::api::routes::messages::resume_tool_loop(&state_clone, &user_id, &chat_id).await
        {
            tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume after service denial");
        }
    });

    Ok(Json(serde_json::json!({ "denied": true })))
}

async fn auth_gate(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let redirect_url = uri
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|pair| pair.strip_prefix("redirect="))
        })
        .unwrap_or("/");

    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let refresh_token = crate::api::cookie::extract_refresh_token_from_cookie_header(cookie_header);

    let Some(refresh_token) = refresh_token else {
        let login_url = build_login_redirect(&state, redirect_url);
        return Redirect::temporary(&login_url).into_response();
    };

    let claims = match state
        .token_service
        .validate(&state.keypair_service, refresh_token)
        .await
    {
        Ok(c) if c.token_type == "refresh" => c,
        _ => {
            let login_url = build_login_redirect(&state, redirect_url);
            return Redirect::temporary(&login_url).into_response();
        }
    };

    let user = crate::core::models::User {
        id: claims.sub,
        username: claims.username,
        email: claims.email,
        name: String::new(),
        password_hash: String::new(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let app_session_jwt = match state
        .token_service
        .create_access_token(&state.keypair_service, &user, "app_session")
        .await
    {
        Ok(jwt) => jwt,
        Err(_) => {
            let login_url = build_login_redirect(&state, redirect_url);
            return Redirect::temporary(&login_url).into_response();
        }
    };

    let secure = state
        .config
        .server
        .base_url
        .as_ref()
        .is_some_and(|u| u.starts_with("https"));

    let cookie = make_app_session_cookie(
        &app_session_jwt,
        state.config.auth.access_token_expiry_secs,
        secure,
    );

    Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header("location", redirect_url)
        .header("set-cookie", cookie)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}


fn build_login_redirect(state: &AppState, app_redirect: &str) -> String {
    let frontend_url = state.config.server.public_frontend_url();
    let base_url = state.config.server.public_base_url();
    if frontend_url.is_empty() {
        return format!("/login?redirect={app_redirect}");
    }
    let gate_url = if base_url.is_empty() {
        format!("/api/auth/apps?redirect={app_redirect}")
    } else {
        format!("{base_url}/api/auth/apps?redirect={app_redirect}")
    };
    let encoded_gate = gate_url.replace('&', "%26");
    format!("{frontend_url}/login?redirect={encoded_gate}")
}

async fn proxy_app_root(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    proxy_app_inner(state, app_id, String::new(), headers, request).await
}

async fn proxy_app_path(
    State(state): State<AppState>,
    Path((app_id, sub_path)): Path<(String, String)>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    proxy_app_inner(state, app_id, sub_path, headers, request).await
}

async fn proxy_app_inner(
    state: AppState,
    app_id: String,
    sub_path: String,
    headers: HeaderMap,
    request: Request,
) -> Response {
    tracing::debug!(app_id = %app_id, sub_path = %sub_path, "Proxy: incoming request");

    let app = match state.app_service.get(&app_id).await {
        Ok(Some(app)) => {
            tracing::debug!(app_id = %app_id, status = ?app.status, kind = %app.kind, "Proxy: app found");
            app
        }
        Ok(None) => {
            tracing::warn!(app_id = %app_id, "Proxy: app not found in DB");
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => {
            tracing::error!(app_id = %app_id, error = %e, "Proxy: DB error looking up app");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let user_id = match authenticate_proxy_request(&state, &headers).await {
        Some(uid) => uid,
        None => {
            let original_uri = request.uri().to_string();
            let gate_url = format!("/api/auth/apps?redirect={original_uri}");
            return Redirect::temporary(&gate_url).into_response();
        }
    };

    if app.user_id != user_id {
        return StatusCode::FORBIDDEN.into_response();
    }

    state.app_service.manager().record_access(&app_id).await;

    match app.kind.as_str() {
        "static" => serve_static(&state, &app, &sub_path, request).await,
        _ => {
            if app.status == AppStatus::Hibernated {
                return handle_hibernated_app(&state, &app, &sub_path, request).await;
            }

            let port = match app.port {
                Some(p) => p,
                None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };

            if !matches!(app.status, AppStatus::Running) {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }

            forward_to_port(port, &sub_path, request).await
        }
    }
}

async fn authenticate_proxy_request(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(auth_header) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some(token) = auth_header.strip_prefix("Bearer ")
        && let Ok(claims) = state
            .token_service
            .validate(&state.keypair_service, token)
            .await
    {
        return Some(claims.sub);
    }

    let cookie_header = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    let token = extract_app_session_from_cookie_header(cookie_header)?;
    state
        .token_service
        .validate(&state.keypair_service, token)
        .await
        .ok()
        .map(|c| c.sub)
}

async fn serve_static(
    state: &AppState,
    app: &crate::app::models::App,
    sub_path: &str,
    request: Request,
) -> Response {
    let static_dir = app.static_dir.as_deref().unwrap_or("dist");
    let workspace_path =
        std::path::Path::new(&state.config.storage.workspaces_path).join(&app.agent_id);
    let serve_path = workspace_path.join(static_dir);

    if !serve_path.exists() {
        tracing::warn!(app_id = %app.id, path = %serve_path.display(), "Proxy: static dir not found");
        return StatusCode::NOT_FOUND.into_response();
    }

    let path = if sub_path.is_empty() { "/" } else { sub_path };
    let (mut parts, body) = request.into_parts();
    parts.uri = path.parse().unwrap_or(Uri::from_static("/"));
    let req = Request::from_parts(parts, body);

    let service = ServeDir::new(&serve_path).append_index_html_on_directories(true);

    match service.oneshot(req).await {
        Ok(resp) => {
            let status = resp.status();
            if status == StatusCode::NOT_FOUND {
                tracing::warn!(app_id = %app.id, serve_path = %serve_path.display(), sub_path = %path, "Proxy: static file not found");
            }
            resp.into_response()
        }
        Err(e) => {
            tracing::error!(app_id = %app.id, error = %e, "Proxy: static serve error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn handle_hibernated_app(
    state: &AppState,
    app: &crate::app::models::App,
    sub_path: &str,
    original_request: Request,
) -> Response {
    let manifest: crate::app::models::AppManifest =
        match serde_json::from_value(app.manifest.clone()) {
            Ok(m) => m,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };

    let command = match &app.command {
        Some(c) => c.clone(),
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let result = state
        .app_service
        .manager()
        .start_app(&app.id, &app.agent_id, &command, &manifest, Vec::new())
        .await;

    match result {
        Ok((port, pid)) => {
            let _ = state
                .app_service
                .update_status(&app.id, AppStatus::Running, Some(port), Some(pid))
                .await;

            let health = manifest
                .health_check
                .as_ref()
                .map(|h| (h.path.clone(), h.effective_initial_delay(), h.effective_timeout()))
                .unwrap_or_else(|| ("/".to_string(), 5, 2));

            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(health.1);

            let hc = crate::app::models::HealthCheck {
                path: health.0,
                interval_secs: Some(1),
                timeout_secs: Some(health.2),
                initial_delay_secs: Some(0),
                failure_threshold: None,
            };

            loop {
                if tokio::time::Instant::now() >= deadline {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                if state.app_service.manager().health_check(port, &hc).await {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            forward_to_port(port, sub_path, original_request).await
        }
        Err(_) => {
            let _ = state
                .app_service
                .update_status(&app.id, AppStatus::Failed, None, None)
                .await;
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn forward_to_port(port: u16, path: &str, original_request: Request) -> Response {
    let uri = if path.is_empty() {
        format!("http://127.0.0.1:{port}/")
    } else {
        format!("http://127.0.0.1:{port}/{path}")
    };

    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };

    let method = original_request.method().clone();

    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    let upstream_resp = match client.request(reqwest_method, &uri).send().await {
        Ok(r) => r,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };

    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);

    let mut builder = Response::builder().status(status);
    for (key, value) in upstream_resp.headers() {
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(key.as_ref())
            && let Ok(val) = HeaderValue::from_bytes(value.as_bytes())
        {
            builder = builder.header(name, val);
        }
    }

    match upstream_resp.bytes().await {
        Ok(body) => builder
            .body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}
