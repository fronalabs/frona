use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, Query, Request, State, WebSocketUpgrade};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::agent::execution::run_agent_loop;
use crate::auth::jwt::JwtService;
use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::inference::InferenceResponse;
use crate::tool::voice::{VoiceCallbackClaims, VoiceSessionClaims};
use tokio_util::sync::CancellationToken;

fn build_twiml(
    ws_url: &str,
    welcome_greeting: Option<&str>,
    hints: Option<&str>,
    voice_id: Option<&str>,
    speech_model: Option<&str>,
) -> String {
    use xml::writer::{EmitterConfig, XmlEvent};

    let mut buf = Vec::new();
    let mut w = EmitterConfig::new()
        .perform_indent(false)
        .write_document_declaration(true)
        .create_writer(&mut buf);

    let mut relay = XmlEvent::start_element("ConversationRelay")
        .attr("url", ws_url)
        .attr("language", "en-US")
        .attr("interruptible", "any")
        .attr("interruptSensitivity", "medium")
        .attr("welcomeGreetingInterruptible", "any");

    if let Some(g) = welcome_greeting {
        relay = relay.attr("welcomeGreeting", g);
    }
    if let Some(v) = voice_id {
        relay = relay.attr("voice", v);
    }
    if let Some(m) = speech_model {
        relay = relay.attr("speechModel", m);
    }
    if let Some(h) = hints {
        relay = relay.attr("hints", h);
    }

    w.write(XmlEvent::start_element("Response")).unwrap();
    w.write(XmlEvent::start_element("Connect")).unwrap();
    w.write(relay).unwrap();
    w.write(XmlEvent::end_element()).unwrap(); // ConversationRelay
    w.write(XmlEvent::end_element()).unwrap(); // Connect
    w.write(XmlEvent::end_element()).unwrap(); // Response

    String::from_utf8(buf).expect("xml-rs always emits valid UTF-8")
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/voice/twilio/callback", post(twilio_callback))
        .route("/api/voice/twilio/ws", get(twilio_ws_handler))
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn verify_jwt<T: DeserializeOwned>(state: &AppState, token: &str) -> Result<T, AppError> {
    let jwt_svc = JwtService::new();
    let kid = jwt_svc
        .decode_unverified_header(token)?
        .kid
        .ok_or_else(|| AppError::Auth("Token missing kid".into()))?;
    let key = state.keypair_service.get_verifying_key(&kid).await?;
    jwt_svc.verify::<T>(token, &key)
}

// ---------------------------------------------------------------------------
// POST /api/voice/twilio/callback
// ---------------------------------------------------------------------------

async fn twilio_callback(
    State(state): State<AppState>,
    Query(q): Query<TokenQuery>,
) -> Response {
    let claims: VoiceCallbackClaims = match verify_jwt(&state, &q.token).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "Voice callback JWT verification failed");
            return (StatusCode::FORBIDDEN, "Invalid token").into_response();
        }
    };

    let user_id = claims.sub.clone();
    let chat_id = claims.chat_id.clone();

    // Mark the Call entity as Active (if it exists)
    let call_id = match state.call_service.find_by_chat_id(&chat_id).await {
        Ok(Some(call)) => {
            match state.call_service.mark_active(&call.id).await {
                Ok(updated) => {
                    tracing::info!(call_id = %updated.id, chat_id = %chat_id, "Call marked Active");
                    Some(updated.id)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to mark call active");
                    Some(call.id)
                }
            }
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to look up call by chat_id");
            None
        }
    };

    let owner = format!("user:{user_id}");
    let expiry_secs = state.config.auth.presign_expiry_secs as i64;
    let (enc_key, kid) = match state.keypair_service.get_signing_key(&owner).await {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get signing key for voice session");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let exp = (Utc::now().timestamp() + expiry_secs) as usize;
    let ws_claims = VoiceSessionClaims {
        sub: user_id.clone(),
        chat_id: chat_id.clone(),
        exp,
        contact_id: claims.contact_id.clone(),
        call_id: call_id.clone(),
    };
    let ws_token = match JwtService::new().sign(&ws_claims, &enc_key, &kid) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "Failed to sign voice session JWT");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let base_url = state.config.voice.callback_base_url.clone()
        .or_else(|| state.config.server.base_url.clone())
        .unwrap_or_else(|| format!("http://localhost:{}", state.config.server.port));
    let ws_base = base_url
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    let ws_url = format!("{ws_base}/api/voice/twilio/ws?token={ws_token}");

    let twiml = build_twiml(
        &ws_url,
        claims.welcome_greeting.as_deref(),
        claims.hints.as_deref(),
        state.config.voice.twilio_voice_id.as_deref(),
        state.config.voice.twilio_speech_model.as_deref(),
    );

    tracing::info!(chat_id = %chat_id, user_id = %user_id, ws_url = %ws_url, "Voice callback: issuing TwiML with ConversationRelay");

    let mut response = twiml.into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    response
}

// ---------------------------------------------------------------------------
// GET /api/voice/twilio/ws  (WebSocket upgrade)
// ---------------------------------------------------------------------------

async fn twilio_ws_handler(
    State(state): State<AppState>,
    Query(q): Query<TokenQuery>,
    req: Request,
) -> Response {
    // Validate JWT BEFORE attempting WebSocket upgrade so we can return 403
    let claims: VoiceSessionClaims = match verify_jwt(&state, &q.token).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "Voice WS JWT verification failed");
            return (StatusCode::FORBIDDEN, "Invalid token").into_response();
        }
    };

    let chat_id = claims.chat_id.clone();
    let user_id = claims.sub.clone();
    let contact_id = claims.contact_id.clone();
    let call_id = claims.call_id.clone();

    let ws = match WebSocketUpgrade::from_request(req, &state).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };

    ws.on_upgrade(move |socket| handle_voice_socket(socket, state, chat_id, user_id, contact_id, call_id))
}

// ---------------------------------------------------------------------------
// WebSocket handler — main voice loop
// ---------------------------------------------------------------------------

async fn handle_voice_socket(
    socket: WebSocket,
    state: AppState,
    chat_id: String,
    user_id: String,
    contact_id: Option<String>,
    call_id: Option<String>,
) {
    state.active_sessions.register(&chat_id).await;
    tracing::debug!(chat_id = %chat_id, "Voice WS session registered in active sessions");
    let (mut ws_send, mut ws_recv) = socket.split();
    let mut last_response = String::new();

    loop {
        let msg = match ws_recv.next().await {
            Some(Ok(Message::Text(raw))) => raw,
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                tracing::warn!(error = %e, chat_id = %chat_id, "Voice WS receive error");
                break;
            }
        };

        let parsed: Value = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = parsed["type"].as_str().unwrap_or("").to_string();
        tracing::debug!(chat_id = %chat_id, msg_type = %msg_type, "Voice WS message received");

        match msg_type.as_str() {
            "setup" => {
                tracing::info!(chat_id = %chat_id, user_id = %user_id, contact_id = ?contact_id, "ConversationRelay connected");
            }
            "interrupt" => {
                tracing::debug!(chat_id = %chat_id, "ConversationRelay interrupt — cancelling active turn");
                state.active_sessions.cancel(&chat_id).await;
            }
            "prompt" if parsed["last"].as_bool() == Some(true) => {
                let voice_prompt = match parsed["voicePrompt"].as_str() {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        tracing::debug!(chat_id = %chat_id, "Ignoring prompt with empty voicePrompt");
                        continue;
                    }
                };

                tracing::info!(chat_id = %chat_id, prompt = %voice_prompt, "Voice turn starting");
                let cancel_token = state.active_sessions.register(&chat_id).await;
                let (response_text, should_hang_up) = match handle_voice_turn(
                    &state,
                    &user_id,
                    &chat_id,
                    &voice_prompt,
                    cancel_token,
                    &mut ws_send,
                    contact_id.as_deref(),
                    call_id.as_deref(),
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        tracing::error!(error = %e, chat_id = %chat_id, "Voice turn failed");
                        continue;
                    }
                };

                tracing::info!(chat_id = %chat_id, response_len = %response_text.len(), should_hang_up = %should_hang_up, "Voice turn complete");
                if !response_text.is_empty() {
                    last_response = response_text.clone();
                    tracing::debug!(chat_id = %chat_id, response = %response_text, "Sending TTS response");
                    let tts = serde_json::json!({
                        "type": "text",
                        "token": response_text,
                        "last": true
                    });
                    if ws_send
                        .send(Message::Text(tts.to_string().into()))
                        .await
                        .is_err()
                    {
                        tracing::warn!(chat_id = %chat_id, "Failed to send TTS response — closing");
                        break;
                    }
                }

                if should_hang_up {
                    // Sleep long enough for the TTS to finish before ending the
                    // call.  Twilio does not send an event when TTS completes, so
                    // this is the only reliable way to avoid cutting off the
                    // farewell message.  Estimate ~2.5 words/second + 1 s buffer,
                    // clamped to [2, 30] seconds.
                    let word_count = response_text.split_whitespace().count();
                    let tts_secs = ((word_count as f64 / 2.5).ceil() as u64 + 1).clamp(2, 30);
                    tracing::info!(chat_id = %chat_id, tts_secs, "Waiting for TTS before hangup");
                    tokio::time::sleep(std::time::Duration::from_secs(tts_secs)).await;
                    tracing::info!(chat_id = %chat_id, "Sending hangup signal to Twilio");
                    let end_msg = serde_json::json!({ "type": "end" });
                    ws_send.send(Message::Text(end_msg.to_string().into())).await.ok();
                    break;
                }
            }
            "prompt" => {
                tracing::debug!(chat_id = %chat_id, "Ignoring partial prompt (last=false)");
            }
            other => {
                tracing::debug!(chat_id = %other, msg_type = %other, "Unhandled ConversationRelay message type");
            }
        }
    }

    tracing::info!(chat_id = %chat_id, "Voice WS session ended");
    state.active_sessions.remove(&chat_id).await;

    if let Some(executor) = state.task_executor()
        && let Ok(Some(task)) = state.task_service.find_by_chat_id(&chat_id).await
        && matches!(task.status, crate::agent::task::models::TaskStatus::InProgress)
    {
        let summary = last_response;

        if let Ok(task) = state.task_service.mark_completed(&task.id, Some(summary.clone())).await {
            executor.deliver_to_source(&task, crate::agent::task::models::TaskStatus::Completed, summary).await;
            executor.broadcast_task_status(&task, "completed", None);
        }
    }
}

/// Run one complete voice turn, handling DTMF and hangup external tool calls in a loop.
/// Returns `(response_text, should_hang_up)`.
#[allow(clippy::too_many_arguments)]
async fn handle_voice_turn(
    state: &AppState,
    user_id: &str,
    chat_id: &str,
    content: &str,
    cancel_token: CancellationToken,
    ws_send: &mut futures::stream::SplitSink<WebSocket, Message>,
    contact_id: Option<&str>,
    call_id: Option<&str>,
) -> Result<(String, bool), AppError> {
    state
        .chat_service
        .save_live_call_message(user_id, chat_id, content, contact_id)
        .await?;

    loop {
        let outcome = run_agent_loop(state, user_id, chat_id, cancel_token.clone()).await?;

        match outcome.response {
            InferenceResponse::ExternalToolPending {
                accumulated_text,
                tool_calls_json,
                tool_results,
                external_tool,
                system_prompt: _,
            } if external_tool.tool_name == "send_dtmf" => {
                tracing::debug!(chat_id = %chat_id, digits = %external_tool.result, "Sending DTMF digits");
                state
                    .chat_service
                    .save_assistant_message_with_tool_calls(
                        chat_id,
                        accumulated_text,
                        Some(tool_calls_json),
                        vec![],
                    )
                    .await
                    .ok();

                for tr in &tool_results {
                    state
                        .chat_service
                        .save_tool_result_message_with_tool(
                            chat_id,
                            &tr.tool_call_id,
                            tr.result.clone(),
                            tr.tool_data.clone(),
                            None,
                        )
                        .await
                        .ok();
                }

                let dtmf_msg = serde_json::json!({
                    "type": "sendDigits",
                    "digits": external_tool.result
                });
                ws_send
                    .send(Message::Text(dtmf_msg.to_string().into()))
                    .await
                    .ok();

                state
                    .chat_service
                    .save_tool_result_message_with_tool(
                        chat_id,
                        &external_tool.tool_call_id,
                        "DTMF sent".to_string(),
                        None,
                        None,
                    )
                    .await
                    .ok();
                // Continue loop — run_agent_loop will see the full history
            }
            InferenceResponse::ExternalToolPending {
                accumulated_text,
                tool_calls_json,
                tool_results,
                external_tool,
                system_prompt: _,
            } if external_tool.tool_name == "hangup_call" => {
                tracing::debug!(chat_id = %chat_id, "Hangup requested by agent");
                state
                    .chat_service
                    .save_assistant_message_with_tool_calls(
                        chat_id,
                        accumulated_text.clone(),
                        Some(tool_calls_json),
                        vec![],
                    )
                    .await
                    .ok();

                for tr in &tool_results {
                    state
                        .chat_service
                        .save_tool_result_message_with_tool(
                            chat_id,
                            &tr.tool_call_id,
                            tr.result.clone(),
                            tr.tool_data.clone(),
                            None,
                        )
                        .await
                        .ok();
                }

                state
                    .chat_service
                    .save_tool_result_message_with_tool(
                        chat_id,
                        &external_tool.tool_call_id,
                        "Call ended".to_string(),
                        None,
                        None,
                    )
                    .await
                    .ok();

                // Mark the call as completed
                if let Some(cid) = call_id
                    && let Err(e) = state.call_service.mark_completed(cid).await
                {
                    tracing::warn!(error = %e, call_id = %cid, "Failed to mark call completed");
                }

                return Ok((accumulated_text, true));
            }
            InferenceResponse::Completed { .. } => {
                let text = outcome.last_segment;
                if !text.is_empty() {
                    state
                        .chat_service
                        .save_assistant_message(chat_id, text.clone())
                        .await
                        .ok();
                }
                return Ok((text, false));
            }
            _ => {
                return Ok((outcome.last_segment, false));
            }
        }
    }
}
