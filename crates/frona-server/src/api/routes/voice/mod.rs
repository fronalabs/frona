mod models;
mod websocket;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::auth::models::Claims;
use crate::auth::token::models::TokenType;
use crate::auth::token::service::CreateTokenRequest;
use crate::core::Principal;
use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::tool::voice::{VoiceCallbackExtensions, VoiceSessionExtensions};

use models::TokenQuery;

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
        .route("/api/voice/twilio/ws", get(websocket::twilio_ws_handler))
}

/// Verify the voice token through the standard `TokenService` — voice tokens
/// are plain access tokens, so they're DB-backed and respect revocation.
pub(super) async fn verify_voice_jwt(state: &AppState, token: &str) -> Result<Claims, AppError> {
    state
        .token_service
        .validate_access(&state.keypair_service, token)
        .await
}

async fn twilio_callback(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    let claims = match verify_voice_jwt(&state, &q.token).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "Voice callback JWT verification failed");
            return (StatusCode::FORBIDDEN, "Invalid token").into_response();
        }
    };

    let ext: VoiceCallbackExtensions = match claims
        .extensions
        .clone()
        .ok_or_else(|| AppError::Validation("voice callback token missing extensions".into()))
        .and_then(|v| {
            serde_json::from_value(v)
                .map_err(|e| AppError::Validation(format!("voice callback extensions: {e}")))
        }) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "Voice callback token extensions invalid");
            return (StatusCode::BAD_REQUEST, "Invalid voice token payload").into_response();
        }
    };

    let user_id = claims.sub.clone();
    let chat_id = ext.chat_id.clone();
    let agent_id = claims.principal.id.clone();

    let call_id = match state.call_service.find_by_chat_id(&chat_id).await {
        Ok(Some(call)) => match state.call_service.mark_active(&call.id).await {
            Ok(updated) => {
                tracing::info!(call_id = %updated.id, chat_id = %chat_id, "Call marked Active");
                Some(updated.id)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to mark call active");
                Some(call.id)
            }
        },
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to look up call by chat_id");
            None
        }
    };

    let ws_ext = match serde_json::to_value(VoiceSessionExtensions {
        chat_id: chat_id.clone(),
        contact_id: ext.contact_id.clone(),
        call_id: call_id.clone(),
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to encode voice session extensions");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let user = match state.user_service.find_by_id(&user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            tracing::warn!(user_id = %user_id, "Voice session: user not found");
            return (StatusCode::UNAUTHORIZED, "User not found").into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = %user_id, "Voice session: user lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let created = match state
        .token_service
        .create_token(
            &state.keypair_service,
            &user,
            CreateTokenRequest {
                token_type: TokenType::Access,
                principal: Principal::agent(&agent_id),
                ttl_secs: state.config.auth.presign_expiry_secs,
                name: "voice_session".into(),
                scopes: Vec::new(),
                refresh_pair_id: None,
                extensions: Some(ws_ext),
            },
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Failed to sign voice session JWT");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let base_url = state
        .config
        .server
        .external_base_url()
        .unwrap_or_else(|| format!("http://localhost:{}", state.config.server.port));
    let ws_base = base_url
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    let ws_url = format!("{ws_base}/api/voice/twilio/ws?token={}", created.jwt);

    let twiml = build_twiml(
        &ws_url,
        ext.welcome_greeting.as_deref(),
        ext.hints.as_deref(),
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
