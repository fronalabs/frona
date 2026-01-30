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

use crate::chat::broadcast::BroadcastEvent;
use crate::chat::message::dto::{MessageResponse, ResolveToolRequest, SendMessageRequest};
use crate::llm::convert::to_rig_messages;
use crate::llm::fallback::stream_inference_with_fallback;
use crate::llm::tool_loop::{self, ToolLoopEvent, ToolLoopEventKind, ToolLoopOutcome};
use crate::agent::models::SandboxSettings;
use crate::tool::browser::tool::BrowserTool;
use crate::tool::web_fetch::WebFetchTool;
use crate::tool::web_search::WebSearchTool;
use crate::tool::cli::CliTool;
use crate::tool::notify_human::NotifyHumanTool;
use crate::tool::registry::AgentToolRegistry;
use crate::tool::remember::RememberTool;
use crate::tool::skill::SkillTool;
use crate::tool::update_entity::UpdateEntityTool;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use super::super::state::AppState;

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
        .route("/api/chats/stream", get(chat_stream))
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
    let messages = state
        .chat_service
        .list_messages(&auth.user_id, &chat_id)
        .await?;
    Ok(Json(messages))
}

async fn cancel_generation(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _chat = state
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
    chat_id: &str,
    allowed_tools: &[String],
    sandbox_config: Option<&SandboxSettings>,
    event_tx: tokio::sync::mpsc::Sender<ToolLoopEvent>,
) -> AgentToolRegistry {
    let mut registry = AgentToolRegistry::new();

    let credential = state
        .credential_service
        .list(user_id)
        .await
        .ok()
        .and_then(|creds| creds.into_iter().next());

    let credential_id = credential.as_ref().map(|c| c.id.clone());

    registry.register(Arc::new(NotifyHumanTool::new(credential_id)));

    registry.register(Arc::new(UpdateEntityTool::new(
        state.db.clone(),
        "agent",
        agent_id,
        user_id,
        "update_agent",
        event_tx,
    )));

    registry.register(Arc::new(RememberTool::new(
        state.memory_service.clone(),
        agent_id.to_string(),
        chat_id.to_string(),
        get_compaction_model_group(state),
    )));

    registry.register(Arc::new(SkillTool::new(
        state.skill_resolver.clone(),
        agent_id.to_string(),
    )));

    if allowed_tools.iter().any(|t| t == "browser") {
        if let Some(credential) = credential {
            registry.register(Arc::new(BrowserTool::new(
                state.browser_session_manager.clone(),
                user_id.to_string(),
                credential.provider,
            )));
        }
    }

    if allowed_tools.iter().any(|t| t == "web_fetch") {
        registry.register(Arc::new(WebFetchTool::new(
            state.browser_session_manager.clone(),
            user_id.to_string(),
        )));
    }

    if allowed_tools.iter().any(|t| t == "web_search") {
        registry.register(Arc::new(WebSearchTool::new(state.search_provider.clone())));
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
    let _chat = state
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

fn get_compaction_model_group(state: &AppState) -> Option<crate::llm::config::ModelGroup> {
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

    let _chat = state
        .chat_service
        .get_chat(&auth.user_id, &chat_id)
        .await
        .map_err(ApiError::from)?;

    let chat = state
        .chat_service
        .find_chat(&chat_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(crate::error::AppError::NotFound("Chat not found".into())))?;

    let stored_messages = state.chat_service.get_stored_messages(&chat_id).await;
    let pending_tool_id = stored_messages.iter().rev().find_map(|m| match &m.tool {
        Some(MessageTool::Question { status: ToolStatus::Pending, .. })
        | Some(MessageTool::HumanInTheLoop { status: ToolStatus::Pending, .. }) => {
            Some(m.id.clone())
        }
        _ => None,
    });

    let agent_config = state
        .chat_service
        .resolve_agent_config(&chat.agent_id)
        .await;
    let base_system_prompt = agent_config.system_prompt;
    let model_group_name = agent_config.model_group;

    let skill_summaries: Vec<(String, String)> = state
        .skill_resolver
        .list(&chat.agent_id)
        .await
        .into_iter()
        .map(|s| (s.name, s.description))
        .collect();

    let system_prompt = match state
        .memory_service
        .build_augmented_system_prompt(
            &base_system_prompt,
            &chat.agent_id,
            chat.space_id.as_deref(),
            &skill_summaries,
        )
        .await
    {
        Ok(prompt) => prompt,
        Err(e) => {
            tracing::warn!(error = %e, agent_id = %chat.agent_id, "Failed to build augmented system prompt, using base");
            base_system_prompt
        }
    };

    let model_group = state
        .chat_service
        .provider_registry()
        .get_model_group(&model_group_name)
        .map_err(|e| ApiError::from(crate::error::AppError::from(e)))?
        .clone();

    if let Some(compaction_group) = get_compaction_model_group(&state) {
        let max_output = model_group.max_tokens.unwrap_or(8192) as usize;
        if let Err(e) = state.memory_service.compact_chat_if_needed(
            &chat_id,
            &system_prompt,
            &model_group.main.model_id,
            model_group.context_window,
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
    rig_history.extend(to_rig_messages(&context_messages));

    let registry = state.chat_service.provider_registry().clone();
    let (tool_event_tx, mut tool_event_rx) = tokio::sync::mpsc::channel::<ToolLoopEvent>(32);
    let tool_registry = build_tool_registry(&state, &chat.agent_id, &auth.user_id, &chat_id, &agent_config.tools, agent_config.sandbox_config.as_ref(), tool_event_tx.clone()).await;
    let user_content = req.content;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);

    let chat_service = state.chat_service.clone();
    let agent_id = chat.agent_id.clone();
    let needs_title = chat.title.is_none();
    let active_sessions = state.active_sessions.clone();
    let cancel_token = active_sessions.register(&chat_id).await;
    if let Some(pending_id) = pending_tool_id {
        let user_response = state
            .chat_service
            .create_stream_user_message(&auth.user_id, &chat_id, &user_content)
            .await
            .map_err(ApiError::from)?;

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
            let rig_history = to_rig_messages(&stored_messages);

            let tool_handle = {
                let registry = registry.clone();
                let model_group = model_group.clone();
                let system_prompt = system_prompt.clone();
                let cancel_token = cancel_token.clone();
                tokio::spawn(async move {
                    tool_loop::run_tool_loop(
                        &registry,
                        &model_group,
                        &system_prompt,
                        rig_history,
                        &tool_registry,
                        tool_event_tx,
                        cancel_token,
                    )
                    .await
                })
            };

            stream_tool_loop_events(&tx, &mut tool_event_rx, tool_handle, &chat_service, &chat_id).await;
            active_sessions.remove(&chat_id_clone).await;
        });
    } else {
        let has_tools = !tool_registry.is_empty();

        let user_response = state
            .chat_service
            .create_stream_user_message(&auth.user_id, &chat_id, &user_content)
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

            if has_tools {
                let user_rig_msg = RigMessage::user(&user_content);
                let mut full_history = rig_history;
                full_history.push(user_rig_msg);

                let tool_handle = {
                    let registry = registry.clone();
                    let model_group = model_group.clone();
                    let system_prompt = system_prompt.clone();
                    let cancel_token = cancel_token.clone();
                    tokio::spawn(async move {
                        tool_loop::run_tool_loop(
                            &registry,
                            &model_group,
                            &system_prompt,
                            full_history,
                            &tool_registry,
                            tool_event_tx,
                            cancel_token,
                        )
                        .await
                    })
                };

                stream_tool_loop_events(&tx, &mut tool_event_rx, tool_handle, &chat_service, &chat_id).await;
            } else {
                let (token_tx, mut token_rx) = tokio::sync::mpsc::channel::<Result<String, crate::llm::LlmError>>(32);

                let user_rig_msg = RigMessage::user(&user_content);

                let stream_handle = tokio::spawn(async move {
                    stream_inference_with_fallback(
                        &registry,
                        &model_group,
                        &system_prompt,
                        rig_history,
                        user_rig_msg,
                        token_tx,
                    )
                    .await
                });

                let mut accumulated = String::new();
                let mut cancelled = false;
                loop {
                    tokio::select! {
                        token_result = token_rx.recv() => {
                            match token_result {
                                Some(Ok(token)) => {
                                    accumulated.push_str(&token);
                                    let token_event = Event::default()
                                        .event("token")
                                        .json_data(serde_json::json!({ "content": token }))
                                        .unwrap();
                                    if tx.send(Ok(token_event)).await.is_err() {
                                        break;
                                    }
                                }
                                Some(Err(e)) => {
                                    let error_event = Event::default()
                                        .event("error")
                                        .json_data(serde_json::json!({ "error": e.to_string() }))
                                        .unwrap();
                                    let _ = tx.send(Ok(error_event)).await;
                                    break;
                                }
                                None => break,
                            }
                        }
                        _ = cancel_token.cancelled() => {
                            cancelled = true;
                            drop(token_rx);
                            break;
                        }
                    }
                }

                let _ = stream_handle.await;

                if !accumulated.is_empty() {
                    tracing::debug!(response = %accumulated, "LLM stream response");
                }

                if cancelled {
                    if !accumulated.is_empty() {
                        let _ = chat_service
                            .save_assistant_message(&chat_id, accumulated)
                            .await;
                    }
                    let cancelled_event = Event::default()
                        .event("cancelled")
                        .json_data(serde_json::json!({ "reason": "User cancelled generation" }))
                        .unwrap();
                    let _ = tx.send(Ok(cancelled_event)).await;
                } else if !accumulated.is_empty()
                    && let Ok(assistant_response) =
                        chat_service.save_assistant_message(&chat_id, accumulated).await
                {
                    let done_event = Event::default()
                        .event("done")
                        .json_data(serde_json::json!({ "message": assistant_response }))
                        .unwrap();
                    let _ = tx.send(Ok(done_event)).await;
                }
            }

            active_sessions.remove(&chat_id_clone).await;
        });
    }

    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn stream_tool_loop_events(
    tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    tool_event_rx: &mut tokio::sync::mpsc::Receiver<ToolLoopEvent>,
    tool_handle: tokio::task::JoinHandle<Result<ToolLoopOutcome, crate::error::AppError>>,
    chat_service: &crate::chat::service::ChatService,
    chat_id: &str,
) {
    let mut accumulated = String::new();
    while let Some(event) = tool_event_rx.recv().await {
        match event.kind {
            ToolLoopEventKind::Text(text) => {
                accumulated.push_str(&text);
                let token_event = Event::default()
                    .event("token")
                    .json_data(serde_json::json!({ "content": text }))
                    .unwrap();
                if tx.send(Ok(token_event)).await.is_err() {
                    break;
                }
            }
            ToolLoopEventKind::ToolCall { name, arguments, description } => {
                let is_human_tool = name == "ask_human_question"
                    || name == "request_human_takeover";
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
            ToolLoopEventKind::ToolResult { name, result } => {
                let result_event = Event::default()
                    .event("tool_result")
                    .json_data(serde_json::json!({
                        "name": name,
                        "result": result,
                    }))
                    .unwrap();
                let _ = tx.send(Ok(result_event)).await;
            }
            ToolLoopEventKind::EntityUpdated { table, record_id, fields } => {
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
            ToolLoopEventKind::RateLimitRetry { retry_after_secs } => {
                let event = Event::default()
                    .event("rate_limit")
                    .json_data(serde_json::json!({ "retry_after_secs": retry_after_secs }))
                    .unwrap();
                let _ = tx.send(Ok(event)).await;
            }
            ToolLoopEventKind::Done(_) => {}
            ToolLoopEventKind::Cancelled(_) => {
                break;
            }
            ToolLoopEventKind::Error(err) => {
                let error_event = Event::default()
                    .event("error")
                    .json_data(serde_json::json!({ "error": err }))
                    .unwrap();
                let _ = tx.send(Ok(error_event)).await;
            }
        }
    }

    match tool_handle.await {
        Ok(Ok(outcome)) => {
            match outcome {
                ToolLoopOutcome::Completed(_) => {
                    if !accumulated.is_empty()
                        && let Ok(assistant_response) =
                            chat_service.save_assistant_message(chat_id, accumulated).await
                    {
                        let done_event = Event::default()
                            .event("done")
                            .json_data(serde_json::json!({ "message": assistant_response }))
                            .unwrap();
                        let _ = tx.send(Ok(done_event)).await;
                    }
                }
                ToolLoopOutcome::Cancelled(_) => {
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
                ToolLoopOutcome::ExternalToolPending {
                    accumulated_text,
                    tool_calls_json,
                    tool_results,
                    external_tool,
                } => {
                    let _ = chat_service
                        .save_assistant_message_with_tool_calls(
                            chat_id,
                            accumulated_text,
                            Some(tool_calls_json),
                        )
                        .await;

                    for tr in &tool_results {
                        if tr.tool_data.is_some() {
                            let _ = chat_service
                                .save_tool_result_message_with_tool(
                                    chat_id,
                                    &tr.tool_call_id,
                                    tr.result.clone(),
                                    tr.tool_data.clone(),
                                )
                                .await;
                        } else {
                            let _ = chat_service
                                .save_tool_result_message(
                                    chat_id,
                                    &tr.tool_call_id,
                                    tr.result.clone(),
                                )
                                .await;
                        }
                    }

                    if let Ok(tool_msg) = chat_service
                        .save_tool_result_message_with_tool(
                            chat_id,
                            &external_tool.tool_call_id,
                            external_tool.result,
                            external_tool.tool_data,
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
        Ok(Err(e)) => tracing::error!(error = %e, "Tool loop failed"),
        Err(e) => tracing::error!(error = %e, "Tool loop panicked"),
    }
}

async fn resume_tool_loop(
    state: &AppState,
    user_id: &str,
    chat_id: &str,
) -> Result<(), crate::error::AppError> {
    let chat = state.chat_service.find_chat(chat_id).await?
        .ok_or_else(|| crate::error::AppError::NotFound("Chat not found".into()))?;

    let agent_config = state.chat_service.resolve_agent_config(&chat.agent_id).await;
    let base_system_prompt = agent_config.system_prompt;
    let model_group_name = agent_config.model_group;

    let skill_summaries: Vec<(String, String)> = state
        .skill_resolver
        .list(&chat.agent_id)
        .await
        .into_iter()
        .map(|s| (s.name, s.description))
        .collect();

    let system_prompt = match state
        .memory_service
        .build_augmented_system_prompt(
            &base_system_prompt,
            &chat.agent_id,
            chat.space_id.as_deref(),
            &skill_summaries,
        )
        .await
    {
        Ok(prompt) => prompt,
        Err(e) => {
            tracing::warn!(error = %e, agent_id = %chat.agent_id, "Failed to build augmented system prompt, using base");
            base_system_prompt
        }
    };

    let stored_messages = state.chat_service.get_stored_messages(chat_id).await;
    let rig_history = to_rig_messages(&stored_messages);

    let model_group = state.chat_service.provider_registry()
        .get_model_group(&model_group_name)?
        .clone();

    let registry = state.chat_service.provider_registry().clone();
    let (tool_event_tx, mut tool_event_rx) = tokio::sync::mpsc::channel::<ToolLoopEvent>(32);
    let tool_registry = build_tool_registry(
        state,
        &chat.agent_id,
        user_id,
        chat_id,
        &agent_config.tools,
        agent_config.sandbox_config.as_ref(),
        tool_event_tx.clone(),
    ).await;

    let cancel_token = state.active_sessions.register(chat_id).await;

    let chat_id_owned = chat_id.to_string();
    let user_id_owned = user_id.to_string();
    let tool_handle = {
        let registry = registry.clone();
        let model_group = model_group.clone();
        let system_prompt = system_prompt.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            tool_loop::run_tool_loop(
                &registry,
                &model_group,
                &system_prompt,
                rig_history,
                &tool_registry,
                tool_event_tx,
                cancel_token,
            )
            .await
        })
    };

    let mut accumulated = String::new();
    while let Some(event) = tool_event_rx.recv().await {
        match event.kind {
            tool_loop::ToolLoopEventKind::Text(text) => {
                accumulated.push_str(&text);
            }
            _ => {}
        }
    }

    match tool_handle.await {
        Ok(Ok(outcome)) => {
            match outcome {
                ToolLoopOutcome::Completed(_) => {
                    if !accumulated.is_empty() {
                        if let Ok(msg) = state.chat_service
                            .save_assistant_message(&chat_id_owned, accumulated)
                            .await
                        {
                            state.broadcast_service.broadcast_chat_message(
                                &user_id_owned,
                                &chat_id_owned,
                                msg,
                            );
                        }
                    }
                }
                ToolLoopOutcome::Cancelled(_) => {
                    if !accumulated.is_empty() {
                        let _ = state.chat_service
                            .save_assistant_message(&chat_id_owned, accumulated)
                            .await;
                    }
                }
                ToolLoopOutcome::ExternalToolPending {
                    accumulated_text,
                    tool_calls_json,
                    tool_results,
                    external_tool,
                } => {
                    if !accumulated.is_empty() {
                        let _ = state.chat_service
                            .save_assistant_message_with_tool_calls(
                                &chat_id_owned,
                                accumulated,
                                Some(tool_calls_json),
                            )
                            .await;
                    } else {
                        let _ = state.chat_service
                            .save_assistant_message_with_tool_calls(
                                &chat_id_owned,
                                accumulated_text,
                                Some(tool_calls_json),
                            )
                            .await;
                    }

                    for tr in &tool_results {
                        if tr.tool_data.is_some() {
                            let _ = state.chat_service
                                .save_tool_result_message_with_tool(
                                    &chat_id_owned,
                                    &tr.tool_call_id,
                                    tr.result.clone(),
                                    tr.tool_data.clone(),
                                )
                                .await;
                        } else {
                            let _ = state.chat_service
                                .save_tool_result_message(
                                    &chat_id_owned,
                                    &tr.tool_call_id,
                                    tr.result.clone(),
                                )
                                .await;
                        }
                    }

                    if let Ok(tool_msg) = state.chat_service
                        .save_tool_result_message_with_tool(
                            &chat_id_owned,
                            &external_tool.tool_call_id,
                            external_tool.result,
                            external_tool.tool_data,
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
            tracing::error!(error = %e, "Background tool loop failed");
        }
        Err(e) => {
            tracing::error!(error = %e, "Background tool loop panicked");
        }
    }

    state.active_sessions.remove(&chat_id_owned).await;
    Ok(())
}

async fn chat_stream(
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
            _ => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
