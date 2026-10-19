mod stream;

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::chat::message::models::{
    MessageQuery, MessageResponse, PaginatedMessagesResponse, ResolveToolRequest,
    SendMessageRequest, UpdateMessageRequest,
};
use crate::credential::presign::presign_response;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/chats/{chat_id}/messages",
            get(list_messages).post(send_message),
        )
        .route(
            "/api/chats/{chat_id}/messages/stream",
            post(stream::stream_message),
        )
        .route(
            "/api/chats/{chat_id}/tool-calls/resolve",
            post(resolve_tool_calls),
        )
        .route("/api/chats/{chat_id}/cancel", post(cancel_generation))
        .route("/api/stream", get(event_stream))
        .route("/api/messages/{id}", axum::routing::patch(patch_message))
}

async fn patch_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMessageRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    let updated = state
        .chat_service
        .update_message_metadata(&auth.user_id, &id, req)
        .await?;
    Ok(Json(updated))
}

async fn send_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<Vec<MessageResponse>>, ApiError> {
    let response = state
        .chat_service
        .send_message(&auth.user_id, &chat_id, req)
        .await?;
    Ok(Json(response))
}

async fn list_messages(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Query(query): Query<MessageQuery>,
) -> Result<Json<PaginatedMessagesResponse>, ApiError> {
    let mut result = state
        .chat_service
        .list_messages_paginated(
            &auth.user_id,
            &chat_id,
            query.before,
            query.after,
            query.limit,
        )
        .await?;

    for msg in &mut result.messages {
        presign_response(&state.presign_service, msg, &auth.user_id, &auth.handle).await;
    }

    Ok(Json(result))
}

async fn cancel_generation(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let chat = state
        .chat_service
        .get_chat(&auth.user_id, &chat_id)
        .await
        .map_err(ApiError::from)?;

    let mut cancelled = state.active_sessions.cancel(&chat_id).await;
    if let Some(task_id) = &chat.task_id {
        cancelled |= state.task_executor.cancel_task(task_id).await;
    }
    Ok(Json(serde_json::json!({ "cancelled": cancelled })))
}

async fn resolve_tool_calls(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(req): Json<ResolveToolRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    use crate::chat::service::ToolResolveResult;

    state
        .chat_service
        .get_chat(&auth.user_id, &chat_id)
        .await
        .map_err(ApiError::from)?;

    let mut last_msg: Option<MessageResponse> = None;

    for resolution in &req.resolutions {
        let te = state
            .chat_service
            .get_tool_call(&resolution.tool_call_id)
            .await
            .map_err(ApiError::from)?
            // Authorizing the URL's chat does not authorize an arbitrary tool
            // call ID. Bind it before either resolution path can have effects.
            .filter(|te| te.chat_id == chat_id)
            .ok_or_else(|| {
                ApiError::from(crate::core::error::AppError::NotFound(format!(
                    "Tool call not found: {}",
                    resolution.tool_call_id
                )))
            })?;

        // Typed HitlResponse routes through the resolve_hitl dispatcher, which
        // runs the tool's on_resume side-effect and synthesizes the result text.
        if let Some(typed) = resolution.hitl_response.clone() {
            let outcome = state
                .harness
                .resolve_and_resume(&resolution.tool_call_id, typed)
                .await
                .map_err(ApiError::from)?;
            if let crate::inference::hitl::ResolveOutcome::Resolved {
                should_resume: true,
                user_id,
                chat_id,
                message_id,
                task_id,
            } = outcome
            {
                let h = state.harness.clone();
                let exec = state.task_executor.clone();
                let cancel_token = if task_id.is_none() {
                    Some(state.active_sessions.register(&chat_id).await)
                } else {
                    None
                };
                tokio::spawn(async move {
                    if let Some(tid) = task_id {
                        let _ = exec.resume_task_by_id(&tid).await;
                    } else if let Some(token) = cancel_token
                        && let Err(e) = h
                            .resume_with_cancel(&user_id, &chat_id, &message_id, token)
                            .await
                    {
                        tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume chat after HITL resolve");
                    }
                });
            }
            if let Ok(Some(msg)) = state.chat_service.find_message(&te.message_id).await {
                last_msg = Some(msg.into());
            }
            continue;
        }

        use crate::chat::message::models::ToolResolutionAction;
        let result = if resolution.action == ToolResolutionAction::Fail {
            state
                .chat_service
                .deny_tool_call(&resolution.tool_call_id, resolution.response.clone())
                .await
                .map_err(ApiError::from)?
        } else {
            state
                .chat_service
                .resolve_tool_call(&resolution.tool_call_id, resolution.response.clone())
                .await
                .map_err(ApiError::from)?
        };

        match result {
            ToolResolveResult::Changed(msg) | ToolResolveResult::AlreadyResolved(msg) => {
                last_msg = Some(msg);
            }
        }
    }

    let msg = last_msg.ok_or_else(|| {
        ApiError::from(crate::core::error::AppError::Validation(
            "No resolutions provided".into(),
        ))
    })?;

    Ok(Json(msg))
}

async fn event_stream(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Event, Infallible>>();

    state
        .broadcast_service
        .register_session(&auth.user_id, tx)
        .await;

    let stream = UnboundedReceiverStream::new(rx);
    Sse::new(stream).keep_alive(KeepAlive::default())
}
