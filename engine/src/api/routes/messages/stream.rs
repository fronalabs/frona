use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use rig::completion::Message as RigMessage;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::chat::broadcast::BroadcastService;
use crate::chat::message::models::SendMessageRequest;
use crate::chat::service::ChatService;
use crate::credential::presign::{PresignService, presign_response, presign_response_by_user_id};
use crate::inference::conversation::{
    ConversationBuilder, ConversationContext, DefaultConversationBuilder, build_user_message,
};
use crate::inference::request::{InferenceRequest, InferenceResponse};
use crate::inference::tool_loop::{InferenceEvent, InferenceEventKind};

use super::super::super::error::ApiError;
use super::super::super::middleware::auth::AuthUser;
use crate::core::state::{ActiveSessions, AppState};

use super::{get_compaction_model_group, sse_event};

fn spawn_inference(
    req: InferenceRequest,
) -> tokio::task::JoinHandle<Result<InferenceResponse, crate::core::error::AppError>> {
    tokio::spawn(async move { crate::inference::inference(req).await })
}

enum ResponseSink {
    Sse {
        tx: tokio::sync::mpsc::UnboundedSender<Result<Event, Infallible>>,
        username: String,
    },
    Broadcast {
        service: BroadcastService,
    },
}

async fn handle_inference_result(
    result: Result<Result<InferenceResponse, crate::core::error::AppError>, tokio::task::JoinError>,
    accumulated: String,
    chat_service: &ChatService,
    chat_id: &str,
    presign_svc: &PresignService,
    user_id: &str,
    sink: ResponseSink,
) {
    match result {
        Ok(Ok(response)) => match response {
            InferenceResponse::Completed { text: _, attachments } => {
                if !accumulated.is_empty()
                    && let Ok(mut msg) = chat_service
                        .save_assistant_message_with_tool_calls(chat_id, accumulated, None, attachments)
                        .await
                {
                    match &sink {
                        ResponseSink::Sse { tx, username } => {
                            presign_response(presign_svc, &mut msg, user_id, username).await;
                            let _ = tx.send(Ok(sse_event("done", serde_json::json!({ "message": msg }))));
                        }
                        ResponseSink::Broadcast { service } => {
                            presign_response_by_user_id(presign_svc, &mut msg, user_id).await;
                            service.broadcast_chat_message(user_id, chat_id, msg);
                        }
                    }
                }
            }
            InferenceResponse::Cancelled(_) => {
                if !accumulated.is_empty() {
                    let _ = chat_service.save_assistant_message(chat_id, accumulated).await;
                }
                if let ResponseSink::Sse { tx, .. } = &sink {
                    let _ = tx
                        .send(Ok(sse_event("cancelled", serde_json::json!({ "reason": "User cancelled generation" }))));

                }
            }
            InferenceResponse::ExternalToolPending {
                accumulated_text,
                tool_calls_json,
                tool_results,
                external_tool,
                system_prompt: _,
            } => {
                let text = if accumulated.is_empty() { accumulated_text } else { accumulated };
                if let Ok(mut tool_msg) = chat_service
                    .save_external_tool_pending(chat_id, text, tool_calls_json, &tool_results, external_tool)
                    .await
                {
                    match &sink {
                        ResponseSink::Sse { tx, username } => {
                            presign_response(presign_svc, &mut tool_msg, user_id, username).await;
                            let _ = tx.send(Ok(sse_event("tool_message", &tool_msg)));
                        }
                        ResponseSink::Broadcast { service } => {
                            presign_response_by_user_id(presign_svc, &mut tool_msg, user_id).await;
                            service.broadcast_chat_message(user_id, chat_id, tool_msg);
                        }
                    }
                }
            }
        },
        Ok(Err(e)) => tracing::error!(error = %e, "Inference failed"),
        Err(e) => tracing::error!(error = %e, "Inference task panicked"),
    }
}

struct MessageStreamSession {
    tx: tokio::sync::mpsc::UnboundedSender<Result<Event, Infallible>>,
    chat_service: ChatService,
    presign_svc: PresignService,
    active_sessions: ActiveSessions,
    chat_id: String,
    user_id: String,
    username: String,
}

impl MessageStreamSession {
    fn send(&self, name: &str, data: impl serde::Serialize) -> bool {
        self.tx.send(Ok(sse_event(name, data))).is_ok()
    }

    async fn send_or_cleanup(&self, name: &str, data: impl serde::Serialize) -> bool {
        if !self.send(name, data) {
            self.cleanup().await;
            return false;
        }
        true
    }

    async fn stream_and_handle(
        &self,
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<InferenceEvent>,
        handle: tokio::task::JoinHandle<Result<InferenceResponse, crate::core::error::AppError>>,
    ) {
        let mut accumulated = String::new();
        while let Some(event) = event_rx.recv().await {
            match event.kind {
                InferenceEventKind::Text(text) => {
                    accumulated.push_str(&text);
                    if !self.send("token", serde_json::json!({ "content": text })) {
                        break;
                    }
                }
                InferenceEventKind::ToolCall { name, arguments, description } => {
                    let is_human_tool = name == "ask_user_question" || name == "request_user_takeover";
                    if !is_human_tool {
                        let _ = self
                            .send("tool_call", serde_json::json!({ "name": name, "arguments": arguments, "description": description }));
                    }
                }
                InferenceEventKind::ToolResult { name, result } => {
                    let _ = self.send("tool_result", serde_json::json!({ "name": name, "result": result }));
                }
                InferenceEventKind::EntityUpdated { table, record_id, fields } => {
                    let _ = self
                        .send("entity_updated", serde_json::json!({ "table": table, "record_id": record_id, "fields": fields }));
                }
                InferenceEventKind::RateLimitRetry { retry_after_ms } => {
                    let _ = self.send("rate_limit", serde_json::json!({ "retry_after_secs": retry_after_ms / 1000 }));
                }
                InferenceEventKind::Done(_) => {}
                InferenceEventKind::Cancelled(_) => break,
                InferenceEventKind::Error(err) => {
                    let _ = self.send("error", serde_json::json!({ "error": err }));
                }
            }
        }

        let sink = ResponseSink::Sse {
            tx: self.tx.clone(),
            username: self.username.clone(),
        };
        handle_inference_result(handle.await, accumulated, &self.chat_service, &self.chat_id, &self.presign_svc, &self.user_id, sink).await;
    }

    async fn cleanup(&self) {
        self.active_sessions.remove(&self.chat_id).await;
    }
}

pub(crate) async fn stream_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    use crate::chat::message::models::{MessageTool, ToolStatus};

    let chat = state
        .chat_service
        .get_chat(&auth.user_id, &chat_id)
        .await
        .map_err(ApiError::from)?;

    let stored_messages = state.chat_service.get_stored_messages(&chat_id).await;
    let pending_tool_id = stored_messages.iter().rev().find_map(|m| match &m.tool {
        Some(MessageTool::Question { status: ToolStatus::Pending, .. })
        | Some(MessageTool::HumanInTheLoop { status: ToolStatus::Pending, .. })
        | Some(MessageTool::VaultApproval { status: ToolStatus::Pending, .. })
        | Some(MessageTool::ServiceApproval { status: ToolStatus::Pending, .. }) => {
            Some(m.id.clone())
        }
        _ => None,
    });

    let cancel_token = state.active_sessions.register(&chat_id).await;
    let mut ctx = crate::chat::session::ChatSessionContext::build(
        &state, &auth.user_id, chat, cancel_token,
    )
    .await
    .map_err(ApiError::from)?;

    if let Some(compaction_group) = get_compaction_model_group(&state) {
        let max_output = ctx.model_group.max_tokens.unwrap_or(8192) as usize;
        if let Err(e) = state.memory_service.compact_chat_if_needed(
            &chat_id,
            &ctx.chat.agent_id,
            &ctx.system_prompt,
            &ctx.model_group.main.model_id,
            ctx.model_group.context_window,
            max_output,
            &compaction_group,
        ).await {
            tracing::warn!(error = %e, "Chat compaction failed, continuing without compaction");
        }
    }

    let (chat_summary, context_messages) = state
        .memory_service
        .get_conversation_context(&chat_id)
        .await
        .unwrap_or((None, stored_messages));

    let mut rig_history = Vec::new();
    if let Some(summary) = &chat_summary {
        rig_history.push(RigMessage::user(format!(
            "[Previous conversation summary]\n{summary}"
        )));
        rig_history.push(RigMessage::assistant(
            "Understood. I have context from our previous conversation. How can I help?",
        ));
    }
    let conv_builder = DefaultConversationBuilder {
        user_service: state.user_service.clone(),
        storage_service: state.storage_service.clone(),
    };
    let conv_ctx = ConversationContext {
        agent_id: ctx.chat.agent_id.clone(),
        model_ref: ctx.model_group.main.clone(),
        user_id: auth.user_id.clone(),
    };
    rig_history.extend(conv_builder.build(&context_messages, &conv_ctx).await);
    ctx.rig_history = rig_history;

    let user_content = req.content;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Event, Infallible>>();

    let agent_id = ctx.chat.agent_id.clone();
    let needs_title = ctx.chat.title.is_none();

    let crate::chat::session::ChatSessionContext {
        system_prompt, model_group, rig_history, registry, tool_registry,
        tool_ctx, cancel_token, mut tool_event_rx, ..
    } = ctx;

    let session = MessageStreamSession {
        tx,
        chat_service: state.chat_service.clone(),
        presign_svc: state.presign_service.clone(),
        active_sessions: state.active_sessions.clone(),
        chat_id: chat_id.clone(),
        user_id: auth.user_id.clone(),
        username: auth.username.clone(),
    };

    if let Some(pending_id) = pending_tool_id {
        let mut user_response = state
            .chat_service
            .create_stream_user_message(&auth.user_id, &chat_id, &user_content, vec![])
            .await
            .map_err(ApiError::from)?;

        presign_response(&session.presign_svc, &mut user_response, &auth.user_id, &auth.username).await;

        let resolved = state
            .chat_service
            .resolve_tool_message(&pending_id, Some(user_content))
            .await
            .map_err(ApiError::from)?;

        let pending_conv_builder = DefaultConversationBuilder {
            user_service: state.user_service.clone(),
            storage_service: state.storage_service.clone(),
        };
        let pending_conv_ctx = ConversationContext {
            agent_id: agent_id.clone(),
            model_ref: model_group.main.clone(),
            user_id: auth.user_id.clone(),
        };

        tokio::spawn(async move {
            if !session.send_or_cleanup("user_message", &user_response).await {
                return;
            }
            if !session.send_or_cleanup("tool_resolved", &resolved).await {
                return;
            }

            let stored_messages = session.chat_service.get_stored_messages(&session.chat_id).await;
            let rig_history = pending_conv_builder.build(&stored_messages, &pending_conv_ctx).await;

            let handle = spawn_inference(InferenceRequest {
                registry, model_group, system_prompt,
                history: rig_history, tool_registry,
                ctx: tool_ctx, cancel_token,
            });

            session.stream_and_handle(&mut tool_event_rx, handle).await;
            session.cleanup().await;
        });
    } else {
        let attachments = req.attachments.clone();

        let mut user_response = state
            .chat_service
            .create_stream_user_message(&auth.user_id, &chat_id, &user_content, req.attachments)
            .await
            .map_err(ApiError::from)?;

        presign_response(&session.presign_svc, &mut user_response, &auth.user_id, &auth.username).await;

        let msg_user_service = state.user_service.clone();
        let msg_storage_service = state.storage_service.clone();

        tokio::spawn(async move {
            if !session.send_or_cleanup("user_message", &user_response).await {
                return;
            }

            if needs_title {
                let svc = session.chat_service.clone();
                let cid = session.chat_id.clone();
                let aid = agent_id.clone();
                let content = user_content.clone();
                let title_tx = session.tx.clone();
                tokio::spawn(async move {
                    match svc.generate_title(&cid, &aid, &content).await {
                        Ok(title) => {
                            let _ = title_tx.send(Ok(sse_event("title", serde_json::json!({ "title": title }))));

                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Title generation failed");
                        }
                    }
                });
            }

            let user_rig_msg = build_user_message(
                &user_content,
                &attachments,
                &msg_user_service,
                &msg_storage_service,
            ).await;
            let mut full_history = rig_history;
            full_history.push(user_rig_msg);

            let handle = spawn_inference(InferenceRequest {
                registry, model_group, system_prompt,
                history: full_history, tool_registry,
                ctx: tool_ctx, cancel_token,
            });

            session.stream_and_handle(&mut tool_event_rx, handle).await;
            session.cleanup().await;
        });
    }

    let stream = UnboundedReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub async fn resume_tool_loop(
    state: &AppState,
    user_id: &str,
    chat_id: &str,
) -> Result<(), crate::core::error::AppError> {
    let chat = state.chat_service.find_chat(chat_id).await?
        .ok_or_else(|| crate::core::error::AppError::NotFound("Chat not found".into()))?;

    let cancel_token = state.active_sessions.register(chat_id).await;
    let crate::chat::session::ChatSessionContext {
        system_prompt, model_group, rig_history, registry,
        tool_registry, tool_ctx, cancel_token,
        mut tool_event_rx, ..
    } = crate::chat::session::ChatSessionContext::build(
        state, user_id, chat, cancel_token,
    ).await?;

    let chat_id_owned = chat_id.to_string();
    let user_id_owned = user_id.to_string();
    let handle = spawn_inference(InferenceRequest {
        registry, model_group, system_prompt,
        history: rig_history, tool_registry,
        ctx: tool_ctx, cancel_token,
    });

    let mut accumulated = String::new();
    while let Some(event) = tool_event_rx.recv().await {
        if let InferenceEventKind::Text(text) = event.kind {
            accumulated.push_str(&text);
        }
    }

    let sink = ResponseSink::Broadcast {
        service: state.broadcast_service.clone(),
    };
    handle_inference_result(
        handle.await, accumulated, &state.chat_service,
        &chat_id_owned, &state.presign_service, &user_id_owned, sink,
    ).await;

    state.active_sessions.remove(&chat_id_owned).await;
    Ok(())
}
