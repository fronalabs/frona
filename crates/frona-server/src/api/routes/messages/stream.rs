use axum::Json;
use axum::extract::{Path, State};

use crate::chat::broadcast::BroadcastEventKind;
use crate::chat::message::models::{MessageCommand, MessageResponse, SendMessageRequest};
use crate::chat::slash::{self, ParsedInvocation};
use crate::core::error::AppError;
use crate::credential::presign::presign_response;
use crate::inference::conversation::DefaultConversationBuilder;
use crate::inference::tool_loop::{InferenceEvent, InferenceEventKind};

use super::super::super::error::ApiError;
use super::super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub(crate) async fn stream_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    let chat = state
        .chat_service
        .get_chat(&auth.user_id, &chat_id)
        .await
        .map_err(ApiError::from)?;

    let pending_tool = state
        .chat_service
        .find_pending_tool_call(&chat_id)
        .await
        .map_err(ApiError::from)?;

    let user_content = req.content;
    let agent_id = chat.agent_id.clone();
    let needs_title = chat.title.is_none();

    if let Some(pending_te) = pending_tool {
        // HITL-resolve path — `/slash` here would be ambiguous (is it a
        // command or a tool response?). Treat as plain text and don't parse.
        let mut user_response = state
            .chat_service
            .create_stream_user_message(&auth.user_id, &chat_id, &user_content, vec![], None)
            .await
            .map_err(ApiError::from)?;

        presign_response(
            &state.presign_service,
            &mut user_response,
            &auth.user_id,
            &auth.handle,
        )
        .await;

        let resolve_result = state
            .chat_service
            .resolve_tool_call(&pending_te.id, Some(user_content))
            .await
            .map_err(ApiError::from)?;

        let resolved_msg = resolve_result.into_message();
        let agent_msg_id = resolved_msg.id.clone();

        let did_flip = state
            .chat_service
            .mark_message_executing(&agent_msg_id)
            .await
            .map_err(ApiError::from)?;

        let event_sender = state.broadcast_service.create_event_sender(
            &auth.user_id,
            &chat_id,
            chat.space_id.clone(),
        );
        event_sender.send(InferenceEvent {
            kind: InferenceEventKind::Resume {
                message: resolved_msg,
            },
        });

        if did_flip {
            let harness = state.harness.clone();
            let user_id = auth.user_id.clone();
            if let Some(task_id) = chat.task_id {
                let executor = state.task_executor.clone();
                tokio::spawn(async move {
                    let _ = executor.resume_task_by_id(&task_id).await;
                });
            } else {
                let cancel_token = state.active_sessions.register(&chat_id).await;
                tokio::spawn(async move {
                    let _ = harness
                        .resume_with_cancel(&user_id, &chat_id, &agent_msg_id, cancel_token)
                        .await;
                });
            }
        }

        Ok(Json(user_response))
    } else {
        let parsed_command = match slash::parse(&user_content) {
            None => None,
            Some(ParsedInvocation::Slash { name, rest }) => {
                Some(resolve_slash_invocation(&state, &auth.user_id, &agent_id, name, rest).await?)
            }
            Some(ParsedInvocation::At { name, rest }) => {
                Some(resolve_at_invocation(&state, &auth.user_id, name, rest).await?)
            }
        };

        let mut user_response = state
            .chat_service
            .create_stream_user_message(
                &auth.user_id,
                &chat_id,
                &user_content,
                req.attachments,
                parsed_command,
            )
            .await
            .map_err(ApiError::from)?;

        presign_response(
            &state.presign_service,
            &mut user_response,
            &auth.user_id,
            &auth.handle,
        )
        .await;

        let agent_msg = state
            .chat_service
            .create_executing_agent_message(&chat_id, &agent_id)
            .await
            .map_err(ApiError::from)?;
        let agent_msg_id = agent_msg.id.clone();

        if needs_title {
            let svc = state.chat_service.clone();
            let cid = chat_id.clone();
            let aid = agent_id.clone();
            let content = user_content.clone();
            let event_sender = state.broadcast_service.create_event_sender(
                &auth.user_id,
                &chat_id,
                chat.space_id.clone(),
            );
            tokio::spawn(async move {
                match svc.generate_title(&cid, &aid, &content).await {
                    Ok(title) => {
                        event_sender.send_kind(BroadcastEventKind::Title { title });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Title generation failed");
                    }
                }
            });
        }

        let harness = state.harness.clone();
        let user_id = auth.user_id.clone();
        let chat_id_clone = chat_id.clone();
        let cancel_token = state.active_sessions.register(&chat_id).await;
        let builder = Box::new(DefaultConversationBuilder {
            user_service: state.user_service.clone(),
            storage_service: state.storage_service.clone(),
            agent_service: state.agent_service.clone(),
        });
        let active_sessions = state.active_sessions.clone();
        tokio::spawn(async move {
            harness
                .run_turn(
                    &user_id,
                    &chat_id_clone,
                    &agent_msg_id,
                    cancel_token,
                    builder,
                    &[],
                    None,
                )
                .await;
            active_sessions.remove(&chat_id_clone).await;
        });

        Ok(Json(user_response))
    }
}

/// Precedence: static commands > skills > agent handles. 400 on miss.
async fn resolve_slash_invocation(
    state: &AppState,
    user_id: &str,
    chat_agent_id: &str,
    name: String,
    rest: String,
) -> Result<MessageCommand, ApiError> {
    if state.harness.commands.get(&name).is_some() {
        return Ok(MessageCommand::Command { name, args: rest });
    }

    let agent = state
        .agent_service
        .find_by_id(chat_agent_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(AppError::NotFound(format!("agent {chat_agent_id}"))))?;
    let user_handle = match state
        .user_service
        .find_by_id(user_id)
        .await
        .map_err(ApiError::from)?
    {
        Some(u) => u.handle,
        None => {
            return Err(ApiError::from(AppError::NotFound(format!(
                "user {user_id}"
            ))));
        }
    };
    let skills = state
        .skill_service
        .list(&user_handle, &agent.handle, agent.skills.as_deref())
        .await;
    if skills.iter().any(|s| s.name == name) {
        return Ok(MessageCommand::Skill { name, prompt: rest });
    }

    if state
        .agent_service
        .find_by_handle(user_id, &name)
        .await
        .map_err(ApiError::from)?
        .is_some()
    {
        return Ok(MessageCommand::Command { name, args: rest });
    }

    Err(ApiError::from(AppError::Validation(format!(
        "unknown command '/{name}'"
    ))))
}

async fn resolve_at_invocation(
    state: &AppState,
    user_id: &str,
    name: String,
    rest: String,
) -> Result<MessageCommand, ApiError> {
    if state
        .agent_service
        .find_by_handle(user_id, &name)
        .await
        .map_err(ApiError::from)?
        .is_some()
    {
        return Ok(MessageCommand::Command { name, args: rest });
    }
    Err(ApiError::from(AppError::Validation(format!(
        "no such agent '@{name}'"
    ))))
}
