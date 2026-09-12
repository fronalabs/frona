use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use tokio::sync::OnceCell;

use crate::chat::channel::adapter::markdown;
use crate::chat::message::models::Message;
use crate::chat::models::Chat;
use crate::core::error::AppError;

use super::super::attachment;
use super::super::error::{ChannelError, ChannelErrorKind};
#[cfg(test)]
use super::super::models::ChannelFactory;
use super::super::models::{ChannelAdapter, ChannelCtx, ExternalMessage, external_chat_id};

#[derive(Debug, Clone, Deserialize)]
pub struct SlackConfig {
    pub bot_token: String,
    pub app_token: String,
}

#[derive(Debug, Clone)]
struct SlackSelfIdentity {
    user_id: String,
    bot_id: Option<String>,
}

#[derive(Clone)]
struct SlackChannelState {
    emit: tokio::sync::mpsc::Sender<ExternalMessage>,
    identity: SlackSelfIdentity,
    hitl: Arc<super::super::HitlDeliveryService>,
    chat_service: crate::chat::service::ChatService,
    bot_token: SlackApiToken,
}

/// Slack `chat.postMessage` text-field hard cap. Slack recommends 4k for
/// readability but accepts up to 40k.
const SLACK_MAX_MESSAGE_LEN: usize = 40_000;

#[derive(crate::ChannelFactory)]
#[channel(id = "slack", from = SlackConfig)]
pub struct SlackAdapter {
    bot_token: SlackApiToken,
    app_token: SlackApiToken,
    client: Arc<SlackHyperClient>,
    identity: OnceCell<SlackSelfIdentity>,
    splitter: super::split::PlainSplitter,
}

impl From<SlackConfig> for SlackAdapter {
    fn from(cfg: SlackConfig) -> Self {
        let connector = SlackClientHyperConnector::new().expect(
            "Slack TLS connector init failed: rustls provider should be installed at startup",
        );
        Self {
            bot_token: SlackApiToken::new(SlackApiTokenValue::from(cfg.bot_token)),
            app_token: SlackApiToken::new(SlackApiTokenValue::from(cfg.app_token)),
            client: Arc::new(SlackClient::new(connector)),
            identity: OnceCell::new(),
            splitter: super::split::PlainSplitter::new(SLACK_MAX_MESSAGE_LEN, None),
        }
    }
}

#[async_trait]
impl ChannelAdapter for SlackAdapter {
    async fn on_connect(&self, ctx: &ChannelCtx) -> Result<(), AppError> {
        let session = self.client.open_session(&self.bot_token);
        let auth = session.auth_test().await.map_err(|e| {
            tracing::warn!(
                channel_id = %ctx.channel.id,
                error = %e,
                "Slack auth.test failed — bot_token rejected (check token is the Bot User OAuth Token, not the App-Level Token)",
            );
            AppError::Validation(format!("Slack auth.test rejected the bot_token: {e}"))
        })?;
        let identity = SlackSelfIdentity {
            user_id: auth.user_id.0.clone(),
            bot_id: auth.bot_id.as_ref().map(|b| b.0.clone()),
        };
        let _ = self.identity.set(identity.clone());

        tracing::info!(
            channel_id = %ctx.channel.id,
            slack_user_id = %identity.user_id,
            slack_team = %auth.team,
            "Slack bot authenticated",
        );

        let env = Arc::new(
            SlackClientEventsListenerEnvironment::new(self.client.clone())
                .with_user_state(SlackChannelState {
                    emit: ctx.emit.clone(),
                    identity,
                    hitl: ctx.hitl.clone(),
                    chat_service: ctx.chat_service.clone(),
                    bot_token: self.bot_token.clone(),
                })
                .with_error_handler(socket_mode_error_handler),
        );
        let callbacks = SlackSocketModeListenerCallbacks::new()
            .with_push_events(handle_push_event)
            .with_interaction_events(handle_interaction_event);
        let listener =
            SlackClientSocketModeListener::new(&SlackClientSocketModeConfig::new(), env, callbacks);

        let app_token = self.app_token.clone();
        let cancel = ctx.cancel.clone();
        let channel_id = ctx.channel.id.clone();
        let signals = ctx.signals.clone();
        tokio::spawn(async move {
            if let Err(e) = listener.listen_for(&app_token).await {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Slack Socket Mode registration failed",
                );
                // Auth failures (bad app token) are terminal; treat other
                // registration errors as transient.
                let msg = format!("Slack Socket Mode listen_for failed: {e}");
                if e.to_string().contains("invalid_auth") || e.to_string().contains("not_authed") {
                    signals.disconnected_terminal(msg);
                } else {
                    signals.disconnected_transient(msg);
                }
                return;
            }
            // Not `serve()`: dropping its future skips the inner `shutdown()`,
            // leaving slack-morphism's WSS tasks alive after cancellation.
            listener.start().await;
            tracing::info!(
                channel_id = %channel_id,
                "Slack channel connected via Socket Mode",
            );
            signals.connected();
            cancel.cancelled().await;
            listener.shutdown().await;
            tracing::info!(
                channel_id = %channel_id,
                "Slack Socket Mode listener stopped (channel cancelled)",
            );
        });

        Ok(())
    }

    async fn on_disconnect(&self, _ctx: &ChannelCtx) -> Result<(), AppError> {
        Ok(())
    }

    async fn on_tool(
        &self,
        tool_call: &crate::inference::tool_call::ToolCall,
        _msg: &Message,
        chat: &Chat,
        ctx: &ChannelCtx,
    ) -> Result<(), ChannelError> {
        let Some(text) = tool_call.turn_text.as_deref() else {
            return Ok(());
        };
        if text.trim().is_empty() {
            return Ok(());
        }
        self.post_message(chat, text, ctx).await
    }

    async fn on_send(
        &self,
        msg: &Message,
        _tool_calls: &[crate::inference::tool_call::ToolCall],
        chat: &Chat,
        ctx: &ChannelCtx,
    ) -> Result<(), ChannelError> {
        let body = crate::chat::channel::render::render_message_body(msg);
        let has_attachments = !msg.attachments.is_empty();

        if !has_attachments {
            if body.trim().is_empty() {
                return Ok(());
            }
            return self.post_message(chat, &body, ctx).await;
        }

        // Build URL buttons for ALL attachments (image native upload deferred
        // in this iteration — see plan A6). Each attachment becomes one URL
        // button labeled "📄 Open {filename}" pointing at the canonical
        // /api/files/... URL.
        let (channel_id, thread_ts) = parse_external_id(external_chat_id(chat)?)?;

        let mut elements: Vec<SlackActionBlockElement> = Vec::new();
        for (i, att) in msg.attachments.iter().enumerate() {
            let url_str =
                match attachment::outbound_url(att, ctx, attachment::ChannelMode::Button).await {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(
                            msg_id = %msg.id,
                            path = %att.path,
                            error = %e,
                            "slack: canonical URL failed; skipping attachment",
                        );
                        continue;
                    }
                };
            let parsed = match url::Url::parse(&url_str) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(
                        msg_id = %msg.id,
                        url = %url_str,
                        error = %e,
                        "slack: unparseable canonical URL; skipping attachment",
                    );
                    continue;
                }
            };
            let label = attachment::button_label(att);
            let btn = SlackBlockButtonElement::new(
                SlackActionId(format!("att:{}:{i}", msg.id)),
                pt(&truncate_label(&label)),
            )
            .with_url(parsed);
            elements.push(btn.into());
        }

        // Compose blocks: body markdown (if any) + actions block (if any
        // buttons were built).
        let mut blocks: Vec<SlackBlock> = Vec::new();
        if !body.trim().is_empty() {
            blocks.push(SlackMarkdownBlock::new(body.clone()).into());
        }
        if !elements.is_empty() {
            blocks.push(SlackActionsBlock::new(elements).into());
        }

        // If nothing to send (no body, no successful buttons), bail.
        if blocks.is_empty() {
            return Ok(());
        }

        let text_fallback = if body.trim().is_empty() {
            "📎 Attachments".to_string()
        } else {
            markdown::to_plain(&body)
        };
        let content = SlackMessageContent::new()
            .with_text(text_fallback)
            .with_blocks(blocks);
        let mut req = SlackApiChatPostMessageRequest::new(channel_id.clone(), content);
        if let Some(ts) = thread_ts {
            req = req.with_thread_ts(ts);
        }
        let session = self.client.open_session(&self.bot_token);
        session
            .chat_post_message(&req)
            .await
            .map_err(|e| classify_slack_error(&e))?;
        Ok(())
    }

    async fn on_pending_hitl(
        &self,
        batch: &[crate::inference::tool_call::ToolCall],
        _msg: &Message,
        chat: &Chat,
        ctx: &ChannelCtx,
    ) -> Result<Vec<crate::inference::hitl::HitlDelivery>, ChannelError> {
        let (channel_id, thread_ts) = parse_external_id(external_chat_id(chat)?)?;
        let session = self.client.open_session(&self.bot_token);
        let mut out = Vec::with_capacity(batch.len());

        for tc in batch {
            let Some(h) = tc.hitl.as_ref() else { continue };
            let kind = crate::chat::channel::hitl::kind_for(&h.request);
            let blocks = build_slack_blocks(&tc.id, &h.prompt, &kind, &h.url);
            let content = SlackMessageContent::new()
                .with_text(h.prompt.clone())
                .with_blocks(blocks);
            let mut req = SlackApiChatPostMessageRequest::new(channel_id.clone(), content);
            if let Some(ts) = thread_ts.clone() {
                req = req.with_thread_ts(ts);
            }
            match session.chat_post_message(&req).await {
                Ok(resp) => out.push(crate::inference::hitl::HitlDelivery {
                    channel_id: ctx.channel.id.clone(),
                    external_message_id: resp.ts.0.clone(),
                    delivered_at: chrono::Utc::now(),
                }),
                Err(e) => {
                    let classified = classify_slack_error(&e);
                    tracing::warn!(
                        channel_id = %ctx.channel.id,
                        tool_call_id = %tc.id,
                        kind = ?classified.kind,
                        error = %e,
                        "Slack on_pending_hitl: send failed",
                    );
                    if !classified.kind.is_terminal() && out.is_empty() {
                        return Err(classified);
                    }
                    break;
                }
            }
        }
        Ok(out)
    }
}

impl SlackAdapter {
    async fn post_message(
        &self,
        chat: &Chat,
        text: &str,
        ctx: &ChannelCtx,
    ) -> Result<(), ChannelError> {
        let (channel_id, thread_ts) = parse_external_id(external_chat_id(chat)?)?;
        let plain = markdown::to_plain(text);
        let sctx = super::split::SplitCtx {
            chat_id: &chat.id,
            user_id: &ctx.channel.user_id,
        };
        let chunks = self.splitter.split(&plain, ctx, sctx).await?;
        if chunks.is_empty() {
            return Ok(());
        }
        let session = self.client.open_session(&self.bot_token);
        for chunk in chunks {
            // Block Kit `markdown` renders CommonMark server-side, not Slack mrkdwn.
            // https://api.slack.com/reference/block-kit/blocks#markdown
            let content = SlackMessageContent::new()
                .with_text(chunk.clone())
                .with_blocks(vec![SlackMarkdownBlock::new(chunk).into()]);
            let mut req = SlackApiChatPostMessageRequest::new(channel_id.clone(), content);
            if let Some(ts) = thread_ts.clone() {
                req = req.with_thread_ts(ts);
            }
            session
                .chat_post_message(&req)
                .await
                .map_err(|e| classify_slack_error(&e))?;
        }
        Ok(())
    }
}

/// Slack's Web API returns only HTTP 200 / 429 (per their docs); failure
/// detail lives in the `ApiError` `code` string. We parse it into typed
/// variants here so the classifier is exhaustive. Unknown codes default to
/// Transient; the cap bounds the retry budget.
#[derive(Debug, Clone)]
enum SlackError {
    /// HTTP 429 supplies `retry_after`; the tier-specific
    /// `ApiError("ratelimited")` returned over HTTP 200 does not.
    RateLimited {
        retry_after: Option<Duration>,
    },
    Other,
    NotInChannel,
    ChannelArchived,
    RestrictedAction,
    MissingScope,
    NotAllowedTokenType,
    UserNotInChannel,
    ChannelNotFound,
    UserNotFound,
    TeamNotFound,
    InvalidAuth,
    NotAuthed,
    TokenRevoked,
    TokenExpired,
    AccountInactive,
    MsgTooLong,
    NoText,
    InvalidBlocks,
    InvalidBlocksFormat,
    InvalidArguments,
    InvalidJson,
    Unknown,
}

impl SlackError {
    fn from_sdk(err: &SlackClientError) -> Self {
        match err {
            SlackClientError::RateLimitError(rl) => Self::RateLimited {
                retry_after: rl.retry_after,
            },
            SlackClientError::ApiError(api) => match api.code.as_str() {
                "ratelimited" => Self::RateLimited { retry_after: None },
                "not_in_channel" => Self::NotInChannel,
                "is_archived" => Self::ChannelArchived,
                "restricted_action" => Self::RestrictedAction,
                "missing_scope" => Self::MissingScope,
                "not_allowed_token_type" => Self::NotAllowedTokenType,
                "user_not_in_channel" => Self::UserNotInChannel,
                "channel_not_found" => Self::ChannelNotFound,
                "user_not_found" | "users_not_found" => Self::UserNotFound,
                "team_not_found" => Self::TeamNotFound,
                "invalid_auth" => Self::InvalidAuth,
                "not_authed" => Self::NotAuthed,
                "token_revoked" => Self::TokenRevoked,
                "token_expired" => Self::TokenExpired,
                "account_inactive" => Self::AccountInactive,
                "msg_too_long" => Self::MsgTooLong,
                "no_text" => Self::NoText,
                "invalid_blocks" => Self::InvalidBlocks,
                "invalid_blocks_format" => Self::InvalidBlocksFormat,
                "invalid_arguments" => Self::InvalidArguments,
                "invalid_json" => Self::InvalidJson,
                _ => Self::Unknown,
            },
            _ => Self::Other,
        }
    }

    fn to_channel_error(&self, msg: String) -> ChannelError {
        use ChannelErrorKind::*;
        match self {
            Self::RateLimited { retry_after } => {
                let mut e = ChannelError::transient(msg);
                if let Some(after) = retry_after {
                    e = e.with_retry_hint(*after);
                }
                e
            }
            Self::Unknown | Self::Other => ChannelError::transient(msg),
            Self::NotInChannel
            | Self::ChannelArchived
            | Self::RestrictedAction
            | Self::MissingScope
            | Self::NotAllowedTokenType
            | Self::UserNotInChannel => ChannelError::terminal(msg, Forbidden),
            Self::ChannelNotFound | Self::UserNotFound | Self::TeamNotFound => {
                ChannelError::terminal(msg, NotFound)
            }
            Self::InvalidAuth
            | Self::NotAuthed
            | Self::TokenRevoked
            | Self::TokenExpired
            | Self::AccountInactive => ChannelError::terminal(msg, Unauthorized),
            Self::MsgTooLong
            | Self::NoText
            | Self::InvalidBlocks
            | Self::InvalidBlocksFormat
            | Self::InvalidArguments
            | Self::InvalidJson => ChannelError::terminal(msg, PayloadInvalid),
        }
    }
}

fn classify_slack_error(err: &SlackClientError) -> ChannelError {
    SlackError::from_sdk(err).to_channel_error(err.to_string())
}

async fn handle_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let (emit, identity) = {
        let guard = states.read().await;
        match guard.get_user_state::<SlackChannelState>() {
            Some(state) => (state.emit.clone(), state.identity.clone()),
            None => return Ok(()),
        }
    };
    if let Some(msg) = convert_message_event(event, &identity)
        && let Err(e) = emit.send(msg).await
    {
        tracing::warn!(error = %e, "Slack inbound emit channel closed");
    }
    Ok(())
}

async fn handle_interaction_event(
    event: SlackInteractionEvent,
    client: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let SlackInteractionEvent::BlockActions(ba) = event else {
        return Ok(());
    };

    let (hitl, chat_service, bot_token) = {
        let guard = states.read().await;
        match guard.get_user_state::<SlackChannelState>() {
            Some(state) => (
                state.hitl.clone(),
                state.chat_service.clone(),
                state.bot_token.clone(),
            ),
            None => return Ok(()),
        }
    };

    let action_id = ba
        .actions
        .as_ref()
        .and_then(|acts| acts.first())
        .map(|a| a.action_id.0.clone())
        .unwrap_or_default();

    let parsed =
        crate::chat::channel::hitl::parse_resolve_callback_data(&action_id, &chat_service).await;

    let (tool_call_id, response) = match parsed {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(action_id = %action_id, error = %e, "Slack action_id parse failed");
            return Ok(());
        }
    };

    let answer_label = crate::chat::channel::hitl::response_display(&response);
    let outcome = hitl.resolve(&tool_call_id, response).await;

    let summary = match &outcome {
        Ok(crate::inference::hitl::ResolveOutcome::Resolved { .. }) => answer_label,
        Ok(crate::inference::hitl::ResolveOutcome::AlreadyResolved) => {
            "Already resolved".to_string()
        }
        Err(e) => format!("Failed: {e}"),
    };

    let (Some(channel), Some(message)) = (ba.channel.as_ref(), ba.message.as_ref()) else {
        return Ok(());
    };
    let original = message.content.text.clone().unwrap_or_default();
    let new_text = format!("{original}\n\n→ {summary}");
    let updated_content = SlackMessageContent::new()
        .with_text(new_text.clone())
        .with_blocks(vec![
            SlackSectionBlock::new()
                .with_text(SlackBlockText::MarkDown(SlackBlockMarkDownText::new(
                    new_text,
                )))
                .into(),
        ]);
    let req = SlackApiChatUpdateRequest::new(
        channel.id.clone(),
        updated_content,
        message.origin.ts.clone(),
    );
    let session = client.open_session(&bot_token);
    if let Err(e) = session.chat_update(&req).await {
        tracing::warn!(error = %e, "Slack chat.update failed");
    }
    Ok(())
}

fn truncate_label(s: &str) -> String {
    const MAX: usize = 75;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX - 1).collect();
        out.push('…');
        out
    }
}

/// Mirror of `discord::build_discord_components`/`telegram::build_inline_keyboard`.
/// Approval → Yes/No + URL fallback (App-deploy needs the manifest link).
/// Choice → per-option buttons only — they ARE the answer. External → URL only.
/// Slack rejects non-http(s) URL buttons, so URL rows silently drop if invalid.
fn build_slack_blocks(
    tcid: &str,
    prompt: &str,
    kind: &crate::chat::channel::hitl::HitlKind,
    url: &str,
) -> Vec<SlackBlock> {
    use crate::chat::channel::hitl::HitlKind;

    let url_button = || -> Option<SlackBlockButtonElement> {
        url::Url::parse(url).ok().map(|u| {
            SlackBlockButtonElement::new(SlackActionId(format!("u:{tcid}")), pt("Open on web →"))
                .with_url(u)
        })
    };

    let prompt_block: SlackBlock = SlackSectionBlock::new()
        .with_text(SlackBlockText::MarkDown(SlackBlockMarkDownText::new(
            prompt.to_string(),
        )))
        .into();

    match kind {
        HitlKind::Approval => {
            let yes = SlackBlockButtonElement::new(SlackActionId(format!("r:{tcid}:y")), pt("Yes"))
                .with_style(SlackBlockButtonStyle::Primary);
            let no = SlackBlockButtonElement::new(SlackActionId(format!("r:{tcid}:n")), pt("No"))
                .with_style(SlackBlockButtonStyle::Danger);
            let mut elements: Vec<SlackActionBlockElement> = vec![yes.into(), no.into()];
            if let Some(b) = url_button() {
                elements.push(b.into());
            }
            vec![prompt_block, SlackActionsBlock::new(elements).into()]
        }
        HitlKind::Choice { options } => {
            // Empty options → no buttons. Slack rejects an actions block with
            // zero elements (`invalid_blocks`). Render prompt only; the
            // resolution mechanism (text reply, web URL) lives outside the
            // message body.
            if options.is_empty() {
                return vec![prompt_block];
            }
            let elements: Vec<SlackActionBlockElement> = options
                .iter()
                .enumerate()
                .map(|(i, opt)| {
                    SlackBlockButtonElement::new(
                        SlackActionId(format!("r:{tcid}:c:{i}")),
                        pt(&truncate_label(opt)),
                    )
                    .into()
                })
                .collect();
            vec![prompt_block, SlackActionsBlock::new(elements).into()]
        }
        HitlKind::External => match url_button() {
            Some(b) => vec![prompt_block, SlackActionsBlock::new(vec![b.into()]).into()],
            None => vec![prompt_block],
        },
    }
}

fn pt(s: &str) -> SlackBlockPlainTextOnly {
    SlackBlockPlainTextOnly::from(s.to_string())
}

fn socket_mode_error_handler(
    err: Box<dyn std::error::Error + Send + Sync + 'static>,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> HttpStatusCode {
    tracing::warn!(error = %err, "Slack Socket Mode handler error");
    HttpStatusCode::OK
}

fn convert_message_event(
    event: SlackPushEventCallback,
    identity: &SlackSelfIdentity,
) -> Option<ExternalMessage> {
    let SlackEventCallbackBody::Message(m) = event.event else {
        return None;
    };

    match m.subtype.as_ref() {
        None => {}
        Some(SlackMessageEventType::ThreadBroadcast) => {} // user-authored, keep
        Some(_) => return None,
    }

    let user_id = m.sender.user.as_ref().map(|u| u.0.as_str()).unwrap_or("");
    if !user_id.is_empty() && user_id == identity.user_id {
        return None;
    }
    if let (Some(self_bot), Some(event_bot)) =
        (identity.bot_id.as_deref(), m.sender.bot_id.as_ref())
        && self_bot == event_bot.0
    {
        return None;
    }

    let channel = m.origin.channel.as_ref()?.0.clone();
    let text = m
        .content
        .as_ref()
        .and_then(|c| c.text.clone())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return None;
    }

    let external_chat_id = format_external_id(
        &channel,
        m.origin.channel_type.as_ref(),
        m.origin.thread_ts.as_ref(),
    );

    let display = m
        .sender
        .user_profile
        .as_ref()
        .and_then(|p| p.real_name.clone().or_else(|| p.display_name.clone()))
        .or_else(|| m.sender.username.clone());

    Some(ExternalMessage {
        external_chat_id,
        sender_address: if user_id.is_empty() {
            m.sender
                .bot_id
                .as_ref()
                .map(|b| b.0.clone())
                .unwrap_or_default()
        } else {
            user_id.to_string()
        },
        sender_external_id: if user_id.is_empty() {
            None
        } else {
            Some(user_id.to_string())
        },
        sender_display_name: display,
        content: text,
        attachments: vec![],
    })
}

fn format_external_id(
    channel: &str,
    channel_type: Option<&SlackChannelType>,
    thread_ts: Option<&SlackTs>,
) -> String {
    let is_dm =
        matches!(channel_type, Some(SlackChannelType(s)) if s == "im") || channel.starts_with('D');
    if is_dm {
        format!("dm:{channel}")
    } else if let Some(ts) = thread_ts {
        format!("group:{channel}:thread:{}", ts.0)
    } else {
        format!("group:{channel}")
    }
}

fn parse_external_id(s: &str) -> Result<(SlackChannelId, Option<SlackTs>), AppError> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        ["dm", id] | ["group", id] if !id.is_empty() => Ok((SlackChannelId(id.to_string()), None)),
        ["group", id, "thread", ts] if !id.is_empty() && !ts.is_empty() => Ok((
            SlackChannelId(id.to_string()),
            Some(SlackTs(ts.to_string())),
        )),
        _ => Err(AppError::Validation(format!(
            "unrecognised Slack external_id format: {s:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Tests that construct `SlackAdapter::from(...)` must call this: rustls
    /// panics without an installed `CryptoProvider`, and tests don't go
    /// through server startup where production installs it.
    fn install_crypto_provider() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            crate::initialize_tls();
        });
    }

    fn identity() -> SlackSelfIdentity {
        SlackSelfIdentity {
            user_id: "U07BOT".into(),
            bot_id: Some("B07BOT".into()),
        }
    }

    #[test]
    fn manifest_declares_required_secret_tokens() {
        let m = SlackAdapterFactory.manifest();
        assert_eq!(m.id, "slack");
        assert_eq!(m.display_name, "Slack");
        let by_name = |n: &str| {
            m.config_fields
                .iter()
                .find(|f| f.name == n)
                .unwrap_or_else(|| panic!("field {n} missing"))
        };
        for n in ["bot_token", "app_token"] {
            let f = by_name(n);
            assert!(f.is_required, "{n} should be required");
            assert!(f.is_secret, "{n} should be marked secret");
        }
    }

    #[test]
    fn factory_create_with_valid_config_succeeds() {
        install_crypto_provider();
        let cfg = json!({"bot_token": "xoxb-abc", "app_token": "xapp-xyz"});
        SlackAdapterFactory
            .create(cfg)
            .expect("valid config should produce a SlackAdapter");
    }

    #[test]
    fn factory_create_rejects_missing_app_token() {
        let cfg = json!({"bot_token": "xoxb-abc"});
        assert!(matches!(
            SlackAdapterFactory.create(cfg),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn factory_create_rejects_missing_bot_token() {
        let cfg = json!({"app_token": "xapp-xyz"});
        assert!(matches!(
            SlackAdapterFactory.create(cfg),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn parse_external_id_dm() {
        let (chan, ts) = parse_external_id("dm:D1ABCD").unwrap();
        assert_eq!(chan.0, "D1ABCD");
        assert!(ts.is_none());
    }

    #[test]
    fn parse_external_id_group() {
        let (chan, ts) = parse_external_id("group:C123456").unwrap();
        assert_eq!(chan.0, "C123456");
        assert!(ts.is_none());
    }

    #[test]
    fn parse_external_id_threaded() {
        let (chan, ts) = parse_external_id("group:C123456:thread:1700000000.000100").unwrap();
        assert_eq!(chan.0, "C123456");
        assert_eq!(ts.unwrap().0, "1700000000.000100");
    }

    #[test]
    fn parse_external_id_rejects_garbage() {
        assert!(parse_external_id("nonsense").is_err());
        assert!(parse_external_id("dm:").is_err());
        assert!(parse_external_id("group:").is_err());
        assert!(parse_external_id("group:C1:thread:").is_err());
    }

    #[test]
    fn format_external_id_dm_by_channel_type() {
        let id = format_external_id("DXYZ", Some(&SlackChannelType("im".to_string())), None);
        assert_eq!(id, "dm:DXYZ");
    }

    #[test]
    fn format_external_id_dm_by_prefix_fallback() {
        let id = format_external_id("D9999", None, None);
        assert_eq!(id, "dm:D9999");
    }

    #[test]
    fn format_external_id_group_with_thread() {
        let id = format_external_id(
            "C1",
            Some(&SlackChannelType("channel".to_string())),
            Some(&SlackTs("1700000000.000100".to_string())),
        );
        assert_eq!(id, "group:C1:thread:1700000000.000100");
    }

    fn message_event(payload: serde_json::Value) -> SlackPushEventCallback {
        let wrapper = json!({
            "team_id": "T1",
            "api_app_id": "A1",
            "event": payload,
            "type": "event_callback",
            "event_id": "Ev1",
            "event_time": 1700000000,
            "event_context": "ctx",
            "authed_users": [],
        });
        serde_json::from_value(wrapper).expect("test event JSON must parse")
    }

    #[test]
    fn convert_dm_text_returns_external_message() {
        let evt = message_event(json!({
            "type": "message",
            "channel": "D1ABCD",
            "channel_type": "im",
            "user": "U07HUMAN",
            "text": "hello",
            "ts": "1700000000.000100",
        }));
        let m = convert_message_event(evt, &identity()).expect("should convert");
        assert_eq!(m.external_chat_id, "dm:D1ABCD");
        assert_eq!(m.sender_address, "U07HUMAN");
        assert_eq!(m.sender_external_id.as_deref(), Some("U07HUMAN"));
        assert_eq!(m.content, "hello");
    }

    #[test]
    fn convert_channel_text_returns_group_external_id() {
        let evt = message_event(json!({
            "type": "message",
            "channel": "C99",
            "channel_type": "channel",
            "user": "U07HUMAN",
            "text": "hi team",
            "ts": "1700000001.000200",
        }));
        let m = convert_message_event(evt, &identity()).expect("should convert");
        assert_eq!(m.external_chat_id, "group:C99");
    }

    #[test]
    fn convert_threaded_reply_includes_thread_ts() {
        let evt = message_event(json!({
            "type": "message",
            "channel": "C99",
            "channel_type": "channel",
            "user": "U07HUMAN",
            "text": "reply",
            "ts": "1700000002.000300",
            "thread_ts": "1700000000.000100",
        }));
        let m = convert_message_event(evt, &identity()).expect("should convert");
        assert_eq!(m.external_chat_id, "group:C99:thread:1700000000.000100");
    }

    #[test]
    fn convert_skips_bot_message_subtype() {
        let evt = message_event(json!({
            "type": "message",
            "subtype": "bot_message",
            "channel": "C99",
            "channel_type": "channel",
            "bot_id": "B07OTHER",
            "text": "from a bot",
            "ts": "1700000003.000400",
        }));
        assert!(convert_message_event(evt, &identity()).is_none());
    }

    #[test]
    fn convert_skips_self_user_id() {
        let evt = message_event(json!({
            "type": "message",
            "channel": "C99",
            "channel_type": "channel",
            "user": "U07BOT",
            "text": "echo of self",
            "ts": "1700000004.000500",
        }));
        assert!(convert_message_event(evt, &identity()).is_none());
    }

    #[test]
    fn convert_skips_self_bot_id() {
        let evt = message_event(json!({
            "type": "message",
            "channel": "C99",
            "channel_type": "channel",
            "user": "U07HUMAN",
            "bot_id": "B07BOT",
            "text": "posted by our app",
            "ts": "1700000005.000600",
        }));
        assert!(convert_message_event(evt, &identity()).is_none());
    }

    #[test]
    fn convert_skips_message_changed_subtype() {
        let evt = message_event(json!({
            "type": "message",
            "subtype": "message_changed",
            "channel": "C99",
            "channel_type": "channel",
            "ts": "1700000006.000700",
        }));
        assert!(convert_message_event(evt, &identity()).is_none());
    }

    #[test]
    fn convert_skips_channel_join_subtype() {
        let evt = message_event(json!({
            "type": "message",
            "subtype": "channel_join",
            "channel": "C99",
            "channel_type": "channel",
            "user": "U07HUMAN",
            "text": "<@U07HUMAN> has joined the channel",
            "ts": "1700000007.000800",
        }));
        assert!(convert_message_event(evt, &identity()).is_none());
    }

    #[test]
    fn convert_skips_empty_text() {
        let evt = message_event(json!({
            "type": "message",
            "channel": "C99",
            "channel_type": "channel",
            "user": "U07HUMAN",
            "text": "   ",
            "ts": "1700000008.000900",
        }));
        assert!(convert_message_event(evt, &identity()).is_none());
    }

    #[test]
    fn convert_prefers_real_name_for_display() {
        let evt = message_event(json!({
            "type": "message",
            "channel": "D1ABCD",
            "channel_type": "im",
            "user": "U07HUMAN",
            "text": "hi",
            "ts": "1700000009.001000",
            "user_profile": {
                "real_name": "Ada Lovelace",
                "display_name": "ada",
            },
        }));
        let m = convert_message_event(evt, &identity()).expect("should convert");
        assert_eq!(m.sender_display_name.as_deref(), Some("Ada Lovelace"));
    }
}
