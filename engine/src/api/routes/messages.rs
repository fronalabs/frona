use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use rig::completion::Message as RigMessage;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::api::files::presign_message;
use crate::auth::jwt::JwtService;
use crate::chat::broadcast::BroadcastEvent;
use crate::chat::message::models::{MessageResponse, ResolveToolRequest, SendMessageRequest};
use crate::inference::convert::{format_content_with_attachments, to_rig_messages};
use crate::inference::request::{InferenceRequest, InferenceResponse};
use crate::inference::tool_loop::{InferenceEvent, InferenceEventKind};
use crate::agent::models::SandboxSettings;
use crate::tool::browser::tool::BrowserTool;
use crate::tool::web_fetch::WebFetchTool;
use crate::tool::web_search::WebSearchTool;
use crate::tool::cli::CliTool;
use crate::tool::notify_human::NotifyHumanTool;
use crate::tool::registry::AgentToolRegistry;
use crate::tool::remember::{RememberTool, RememberUserFactTool};
use crate::tool::skill::SkillTool;
use crate::tool::delegate::DelegateTaskTool;
use crate::tool::heartbeat::HeartbeatTool;
use crate::tool::produce_file::ProduceFileTool;
use crate::tool::read_file::ReadFileTool;
use crate::tool::request_credentials::RequestCredentialsTool;
use crate::tool::schedule::ScheduleTaskTool;
use crate::tool::time::TimeTool;
use crate::tool::update_entity::UpdateEntityTool;
use crate::tool::update_identity::UpdateIdentityTool;

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
            post(stream_message),
        )
        .route(
            "/api/chats/{chat_id}/messages/{message_id}/resolve",
            post(resolve_tool_message),
        )
        .route(
            "/api/chats/{chat_id}/cancel",
            post(cancel_generation),
        )
        .route("/api/stream", get(event_stream))
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
) -> Result<Json<Vec<MessageResponse>>, ApiError> {
    let mut messages = state
        .chat_service
        .list_messages(&auth.user_id, &chat_id)
        .await?;

    let jwt_svc = JwtService::new();
    for msg in &mut messages {
        presign_message(
            msg,
            &state.keypair_service,
            &jwt_svc,
            &auth.user_id,
            &auth.username,
            &state.config.server.issuer_url,
            state.config.auth.presign_expiry_secs,
        )
        .await;
    }

    Ok(Json(messages))
}

async fn cancel_generation(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .chat_service
        .get_chat(&auth.user_id, &chat_id)
        .await
        .map_err(ApiError::from)?;

    let cancelled = state.active_sessions.cancel(&chat_id).await;
    Ok(Json(serde_json::json!({ "cancelled": cancelled })))
}

pub async fn build_tool_registry(
    state: &AppState,
    agent_id: &str,
    user_id: &str,
    username: &str,
    chat_id: &str,
    allowed_tools: &[String],
    sandbox_config: Option<&SandboxSettings>,
) -> AgentToolRegistry {
    let mut registry = AgentToolRegistry::new();

    let credential = state
        .vault_service
        .list_credentials(user_id)
        .await
        .ok()
        .and_then(|creds| creds.into_iter().next());

    let credential_id = credential.as_ref().map(|c| c.id.clone());

    let prompts = state.prompts.clone();

    registry.register(Arc::new(TimeTool::new(prompts.clone())));
    registry.register(Arc::new(NotifyHumanTool::new(credential_id, prompts.clone())));

    registry.register(Arc::new(ReadFileTool::new(
        state.config.as_ref().clone(),
        prompts.clone(),
    )));

    let workspace_path = std::path::Path::new(&state.config.storage.workspaces_path).join(agent_id);
    registry.register(Arc::new(ProduceFileTool::new(
        agent_id.to_string(),
        workspace_path,
        prompts.clone(),
    )));

    registry.register(Arc::new(UpdateEntityTool::new(
        state.db.clone(),
        "agent",
        agent_id,
        user_id,
        "update_agent",
    )));

    registry.register(Arc::new(UpdateIdentityTool::new(
        state.db.clone(),
        agent_id,
        user_id,
        prompts.clone(),
    )));

    registry.register(Arc::new(RememberTool::new(
        state.memory_service.clone(),
        agent_id.to_string(),
        chat_id.to_string(),
        get_compaction_model_group(state),
        prompts.clone(),
    )));

    registry.register(Arc::new(RememberUserFactTool::new(
        state.memory_service.clone(),
        user_id.to_string(),
        chat_id.to_string(),
        get_compaction_model_group(state),
        prompts.clone(),
    )));

    registry.register(Arc::new(SkillTool::new(
        state.skill_resolver.clone(),
        agent_id.to_string(),
        prompts.clone(),
    )));

    if allowed_tools.iter().any(|t| t == "browser")
        && let Some(credential) = credential
    {
        registry.register(Arc::new(BrowserTool::new(
            state.browser_session_manager.clone(),
            username.to_string(),
            credential.provider,
        )));
    }

    if allowed_tools.iter().any(|t| t == "web_fetch") {
        registry.register(Arc::new(WebFetchTool::new(
            state.browser_session_manager.clone(),
            username.to_string(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "web_search") {
        registry.register(Arc::new(WebSearchTool::new(state.search_provider.clone(), prompts.clone())));
    }

    let agent_repo: Arc<dyn crate::agent::repository::AgentRepository> =
        Arc::new(crate::api::repo::generic::SurrealRepo::new(state.db.clone()));

    if allowed_tools.iter().any(|t| t == "delegate")
        && let Some(executor) = state.task_executor()
    {
        let chat = state.chat_service.find_chat(chat_id).await.ok().flatten();
        let space_id = chat.and_then(|c| c.space_id);

        registry.register(Arc::new(DelegateTaskTool::new(
            state.task_service.clone(),
            agent_repo.clone(),
            executor,
            state.broadcast_service.clone(),
            user_id.to_string(),
            agent_id.to_string(),
            chat_id.to_string(),
            space_id,
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "schedule") {
        registry.register(Arc::new(ScheduleTaskTool::new(
            state.task_service.clone(),
            agent_repo.clone(),
            user_id.to_string(),
            agent_id.to_string(),
            chat_id.to_string(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "heartbeat") {
        registry.register(Arc::new(HeartbeatTool::new(
            state.agent_service.clone(),
            state.agent_workspaces.clone(),
            agent_id.to_string(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "request_credentials") {
        registry.register(Arc::new(RequestCredentialsTool::new(
            state.vault_service.clone(),
            prompts.clone(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "make_voice_call") {
        registry.register(Arc::new(crate::tool::voice::VoiceCallTool {
            provider: state.voice_provider.clone(),
            prompts: prompts.clone(),
            contact_service: state.contact_service.clone(),
            call_service: state.call_service.clone(),
        }));
    }

    if allowed_tools.iter().any(|t| t == "send_dtmf") {
        registry.register(Arc::new(crate::tool::voice::SendDtmfTool {
            prompts: prompts.clone(),
        }));
    }

    if allowed_tools.iter().any(|t| t == "hangup_call") {
        registry.register(Arc::new(crate::tool::voice::HangupCallTool {
            prompts: prompts.clone(),
        }));
    }

    let skill_dirs: Vec<(String, String)> = state
        .skill_resolver
        .list(agent_id)
        .await
        .into_iter()
        .filter_map(|s| {
            state
                .skill_resolver
                .skill_dir_path(agent_id, &s.name)
                .map(|p| {
                    let abs = std::fs::canonicalize(&p)
                        .map(|c| c.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| p.to_string_lossy().into_owned());
                    (format!("skills/{}/", s.name), abs)
                })
        })
        .collect();

    let defaults = sandbox_config.cloned().unwrap_or_default();
    tracing::info!(cli_tools_count = state.cli_tools_config.len(), ?allowed_tools, "Building tool registry");
    for tool_config in state.cli_tools_config.iter() {
        if allowed_tools.iter().any(|t| t == &tool_config.name) {
            tracing::info!(tool = %tool_config.name, "Registering CLI tool");
            registry.register(Arc::new(CliTool::new(
                tool_config.clone(),
                state.workspace_manager.clone(),
                agent_id.to_string(),
                defaults.network_access,
                defaults.allowed_network_destinations.clone(),
            ).with_skill_dirs(skill_dirs.clone())));
        }
    }

    let tool_names: Vec<&str> = registry.definitions.iter().map(|d| d.name.as_str()).collect();
    tracing::info!(
        ?tool_names,
        cli_configs = state.cli_tools_config.len(),
        ?allowed_tools,
        "Tool registry built"
    );

    registry
}

async fn resolve_tool_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((chat_id, message_id)): Path<(String, String)>,
    Json(req): Json<ResolveToolRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    state
        .chat_service
        .get_chat(&auth.user_id, &chat_id)
        .await
        .map_err(ApiError::from)?;

    let updated = state
        .chat_service
        .resolve_tool_message(&message_id, req.response.clone())
        .await
        .map_err(ApiError::from)?;

    let user_id = auth.user_id.clone();
    let state = state.clone();

    tokio::spawn(async move {
        if let Err(e) = resume_tool_loop(
            &state,
            &user_id,
            &chat_id,
        ).await {
            tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume tool loop");
        }
    });

    Ok(Json(updated))
}

fn get_compaction_model_group(state: &AppState) -> Option<crate::inference::config::ModelGroup> {
    let registry = state.chat_service.provider_registry();
    if let Ok(group) = registry.get_model_group("compaction") {
        return Some(group.clone());
    }
    if let Ok(group) = registry.get_model_group("primary") {
        return Some(group.clone());
    }
    None
}

async fn stream_message(
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
        | Some(MessageTool::VaultApproval { status: ToolStatus::Pending, .. }) => {
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
    rig_history.extend(to_rig_messages(&context_messages, &ctx.chat.agent_id));
    ctx.rig_history = rig_history;

    let user_content = req.content;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);

    let chat_service = state.chat_service.clone();
    let agent_id = ctx.chat.agent_id.clone();
    let needs_title = ctx.chat.title.is_none();
    let active_sessions = state.active_sessions.clone();

    let crate::chat::session::ChatSessionContext {
        system_prompt, model_group, rig_history, registry, tool_registry,
        tool_ctx, cancel_token, mut tool_event_rx, ..
    } = ctx;
    let presign_issuer = state.config.server.issuer_url.clone();
    let presign_expiry = state.config.auth.presign_expiry_secs;
    let presign_keypair = state.keypair_service.clone();

    if let Some(pending_id) = pending_tool_id {
        let mut user_response = state
            .chat_service
            .create_stream_user_message(&auth.user_id, &chat_id, &user_content, vec![])
            .await
            .map_err(ApiError::from)?;

        presign_message(
            &mut user_response,
            &presign_keypair,
            &JwtService::new(),
            &auth.user_id,
            &auth.username,
            &presign_issuer,
            presign_expiry,
        )
        .await;

        let resolved = state
            .chat_service
            .resolve_tool_message(&pending_id, Some(user_content))
            .await
            .map_err(ApiError::from)?;

        let chat_id_clone = chat_id.clone();
        tokio::spawn(async move {
            let user_event = Event::default()
                .event("user_message")
                .json_data(&user_response)
                .unwrap();
            if tx.send(Ok(user_event)).await.is_err() {
                active_sessions.remove(&chat_id).await;
                return;
            }

            let resolve_event = Event::default()
                .event("tool_resolved")
                .json_data(&resolved)
                .unwrap();
            if tx.send(Ok(resolve_event)).await.is_err() {
                active_sessions.remove(&chat_id).await;
                return;
            }

            let stored_messages = chat_service.get_stored_messages(&chat_id).await;
            let rig_history = to_rig_messages(&stored_messages, &agent_id);

            let handle = tokio::spawn(async move {
                crate::inference::inference(InferenceRequest {
                    registry, model_group, system_prompt,
                    history: rig_history, tool_registry,
                    ctx: tool_ctx, cancel_token,
                }).await
            });

            stream_inference_events(&tx, &mut tool_event_rx, handle, &chat_service, &chat_id).await;
            active_sessions.remove(&chat_id_clone).await;
        });
    } else {
        let attachments = req.attachments.clone();

        let mut user_response = state
            .chat_service
            .create_stream_user_message(&auth.user_id, &chat_id, &user_content, req.attachments)
            .await
            .map_err(ApiError::from)?;

        presign_message(
            &mut user_response,
            &presign_keypair,
            &JwtService::new(),
            &auth.user_id,
            &auth.username,
            &presign_issuer,
            presign_expiry,
        )
        .await;

        let chat_id_clone = chat_id.clone();
        tokio::spawn(async move {
            let user_event = Event::default()
                .event("user_message")
                .json_data(&user_response)
                .unwrap();
            if tx.send(Ok(user_event)).await.is_err() {
                active_sessions.remove(&chat_id).await;
                return;
            }

            if needs_title {
                let svc = chat_service.clone();
                let cid = chat_id.clone();
                let aid = agent_id.clone();
                let content = user_content.clone();
                let title_tx = tx.clone();
                tokio::spawn(async move {
                    match svc.generate_title(&cid, &aid, &content).await {
                        Ok(title) => {
                            let title_event = Event::default()
                                .event("title")
                                .json_data(serde_json::json!({ "title": title }))
                                .unwrap();
                            let _ = title_tx.send(Ok(title_event)).await;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Title generation failed");
                        }
                    }
                });
            }

            let user_rig_msg = RigMessage::user(format_content_with_attachments(&user_content, &attachments));
            let mut full_history = rig_history;
            full_history.push(user_rig_msg);

            let handle = tokio::spawn(async move {
                crate::inference::inference(InferenceRequest {
                    registry, model_group, system_prompt,
                    history: full_history, tool_registry,
                    ctx: tool_ctx, cancel_token,
                }).await
            });

            stream_inference_events(&tx, &mut tool_event_rx, handle, &chat_service, &chat_id).await;
            active_sessions.remove(&chat_id_clone).await;
        });
    }

    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn stream_inference_events(
    tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    event_rx: &mut tokio::sync::mpsc::Receiver<InferenceEvent>,
    handle: tokio::task::JoinHandle<Result<InferenceResponse, crate::core::error::AppError>>,
    chat_service: &crate::chat::service::ChatService,
    chat_id: &str,
) {
    let mut accumulated = String::new();
    while let Some(event) = event_rx.recv().await {
        match event.kind {
            InferenceEventKind::Text(text) => {
                accumulated.push_str(&text);
                let token_event = Event::default()
                    .event("token")
                    .json_data(serde_json::json!({ "content": text }))
                    .unwrap();
                if tx.send(Ok(token_event)).await.is_err() {
                    break;
                }
            }
            InferenceEventKind::ToolCall { name, arguments, description } => {
                let is_human_tool = name == "ask_user_question"
                    || name == "request_user_takeover";
                if !is_human_tool {
                    let tool_event = Event::default()
                        .event("tool_call")
                        .json_data(serde_json::json!({
                            "name": name,
                            "arguments": arguments,
                            "description": description,
                        }))
                        .unwrap();
                    let _ = tx.send(Ok(tool_event)).await;
                }
            }
            InferenceEventKind::ToolResult { name, result } => {
                let result_event = Event::default()
                    .event("tool_result")
                    .json_data(serde_json::json!({
                        "name": name,
                        "result": result,
                    }))
                    .unwrap();
                let _ = tx.send(Ok(result_event)).await;
            }
            InferenceEventKind::EntityUpdated { table, record_id, fields } => {
                let update_event = Event::default()
                    .event("entity_updated")
                    .json_data(serde_json::json!({
                        "table": table,
                        "record_id": record_id,
                        "fields": fields,
                    }))
                    .unwrap();
                let _ = tx.send(Ok(update_event)).await;
            }
            InferenceEventKind::RateLimitRetry { retry_after_ms } => {
                let event = Event::default()
                    .event("rate_limit")
                    .json_data(serde_json::json!({ "retry_after_secs": retry_after_ms / 1000 }))
                    .unwrap();
                let _ = tx.send(Ok(event)).await;
            }
            InferenceEventKind::Done(_) => {}
            InferenceEventKind::Cancelled(_) => {
                break;
            }
            InferenceEventKind::Error(err) => {
                let error_event = Event::default()
                    .event("error")
                    .json_data(serde_json::json!({ "error": err }))
                    .unwrap();
                let _ = tx.send(Ok(error_event)).await;
            }
        }
    }

    match handle.await {
        Ok(Ok(response)) => {
            match response {
                InferenceResponse::Completed { text: _, attachments } => {
                    if !accumulated.is_empty()
                        && let Ok(assistant_response) =
                            chat_service.save_assistant_message_with_tool_calls(
                                chat_id, accumulated, None, attachments,
                            ).await
                    {
                        let done_event = Event::default()
                            .event("done")
                            .json_data(serde_json::json!({ "message": assistant_response }))
                            .unwrap();
                        let _ = tx.send(Ok(done_event)).await;
                    }
                }
                InferenceResponse::Cancelled(_) => {
                    if !accumulated.is_empty() {
                        let _ = chat_service
                            .save_assistant_message(chat_id, accumulated)
                            .await;
                    }
                    let cancelled_event = Event::default()
                        .event("cancelled")
                        .json_data(serde_json::json!({ "reason": "User cancelled generation" }))
                        .unwrap();
                    let _ = tx.send(Ok(cancelled_event)).await;
                }
                InferenceResponse::ExternalToolPending {
                    accumulated_text,
                    tool_calls_json,
                    tool_results,
                    external_tool,
                    system_prompt: _,
                } => {
                    if let Ok(tool_msg) = chat_service
                        .save_external_tool_pending(
                            chat_id,
                            accumulated_text,
                            tool_calls_json,
                            &tool_results,
                            external_tool,
                        )
                        .await
                    {
                        let tool_event = Event::default()
                            .event("tool_message")
                            .json_data(&tool_msg)
                            .unwrap();
                        let _ = tx.send(Ok(tool_event)).await;
                    }
                }
            }
        }
        Ok(Err(e)) => tracing::error!(error = %e, "Inference failed"),
        Err(e) => tracing::error!(error = %e, "Inference task panicked"),
    }
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
    let handle = tokio::spawn(async move {
        crate::inference::inference(InferenceRequest {
            registry, model_group, system_prompt,
            history: rig_history, tool_registry,
            ctx: tool_ctx, cancel_token,
        }).await
    });

    let mut accumulated = String::new();
    while let Some(event) = tool_event_rx.recv().await {
        if let InferenceEventKind::Text(text) = event.kind {
            accumulated.push_str(&text);
        }
    }

    match handle.await {
        Ok(Ok(response)) => {
            match response {
                InferenceResponse::Completed { text: _, attachments } => {
                    if !accumulated.is_empty()
                        && let Ok(msg) = state.chat_service
                            .save_assistant_message_with_tool_calls(
                                &chat_id_owned, accumulated, None, attachments,
                            )
                            .await
                    {
                        state.broadcast_service.broadcast_chat_message(
                            &user_id_owned,
                            &chat_id_owned,
                            msg,
                        );
                    }
                }
                InferenceResponse::Cancelled(_) => {
                    if !accumulated.is_empty() {
                        let _ = state.chat_service
                            .save_assistant_message(&chat_id_owned, accumulated)
                            .await;
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
                    if let Ok(tool_msg) = state.chat_service
                        .save_external_tool_pending(
                            &chat_id_owned,
                            text,
                            tool_calls_json,
                            &tool_results,
                            external_tool,
                        )
                        .await
                    {
                        state.broadcast_service.broadcast_chat_message(
                            &user_id_owned,
                            &chat_id_owned,
                            tool_msg,
                        );
                    }
                }
            }
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Background inference failed");
        }
        Err(e) => {
            tracing::error!(error = %e, "Background inference task panicked");
        }
    }

    state.active_sessions.remove(&chat_id_owned).await;
    Ok(())
}

pub async fn build_agent_summaries_from_state(
    state: &AppState,
    user_id: &str,
    current_agent_id: &str,
    tools: &[String],
) -> Vec<(String, String)> {
    if !tools.iter().any(|t| t == "delegate") {
        return Vec::new();
    }

    let agents = match state.agent_service.list(user_id).await {
        Ok(agents) => agents,
        Err(_) => return Vec::new(),
    };

    agents
        .into_iter()
        .filter(|a| a.id != current_agent_id && a.enabled)
        .map(|a| (a.name, a.description))
        .collect()
}

async fn event_stream(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let user_id = auth.user_id.clone();
    let rx = state.broadcast_service.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let user_id = user_id.clone();
        match result {
            Ok(BroadcastEvent::ChatMessage { user_id: uid, chat_id, message }) if uid == user_id => {
                Some(Ok(Event::default()
                    .event("chat_message")
                    .json_data(serde_json::json!({
                        "chat_id": chat_id,
                        "message": message,
                    }))
                    .unwrap()))
            }
            Ok(BroadcastEvent::TaskUpdate {
                user_id: uid,
                task_id,
                status,
                title,
                chat_id,
                source_chat_id,
                result_summary,
            }) if uid == user_id => {
                Some(Ok(Event::default()
                    .event("task_update")
                    .json_data(serde_json::json!({
                        "task_id": task_id,
                        "status": status,
                        "title": title,
                        "chat_id": chat_id,
                        "source_chat_id": source_chat_id,
                        "result_summary": result_summary,
                    }))
                    .unwrap()))
            }
            Ok(BroadcastEvent::InferenceCount { count }) => {
                Some(Ok(Event::default()
                    .event("inference_count")
                    .json_data(serde_json::json!({ "count": count }))
                    .unwrap()))
            }
            _ => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
