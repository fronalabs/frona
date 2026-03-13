mod models;
mod proxy;

use axum::extract::{Path, State};
use axum::routing::{any, get, post};
use axum::{Json, Router};

use crate::app::models::{App, AppResponse};
use crate::core::state::AppState;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;

use models::ServiceActionRequest;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/apps", get(list_apps))
        .route("/api/apps/{id}", get(get_app).delete(delete_app))
        .route("/api/apps/{id}/stop", post(stop_app))
        .route("/api/apps/{id}/restart", post(restart_app))
        .route("/api/apps/approve", post(approve_service))
        .route("/api/apps/deny", post(deny_service))
        .route("/api/auth/apps", get(proxy::auth_gate))
        .route("/apps/{id}", any(proxy::proxy_app_root))
        .route("/apps/{id}/", any(proxy::proxy_app_root))
        .route("/apps/{id}/{*path}", any(proxy::proxy_app_path))
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
