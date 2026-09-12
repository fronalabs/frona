use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serenity::Error as SerenityError;
use serenity::all::{
    ButtonStyle, ChannelId, Client, Context, CreateActionRow, CreateAttachment, CreateButton,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, EventHandler,
    GatewayIntents, Http, Interaction, Message as DiscordMessage, UserId,
};
use serenity::http::HttpError;

use crate::chat::message::models::Message;
use crate::chat::models::Chat;
use crate::core::error::AppError;

use super::super::attachment;
use super::super::error::{ChannelError, ChannelErrorKind};
#[cfg(test)]
use super::super::models::ChannelFactory;
use super::super::models::{ChannelAdapter, ChannelCtx, ExternalMessage, external_chat_id};
use super::super::typing::TypingIndicator;

// Discord API cap. https://discord.com/developers/docs/resources/message
const DISCORD_MAX_MESSAGE_LEN: usize = 2000;

/// Discord's typing indicator auto-fades in ~10s. Refresh a bit early so a
/// long inference keeps showing "typing…" continuously.
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Deserialize)]
pub struct DiscordConfig {
    pub bot_token: String,
}

#[derive(crate::ChannelFactory)]
#[channel(id = "discord", from = DiscordConfig)]
pub struct DiscordAdapter {
    bot_token: String,
    http: Arc<Http>,
    self_id: Arc<OnceLock<UserId>>,
    typing: TypingIndicator,
    splitter: super::split::MarkdownSplitter,
}

impl From<DiscordConfig> for DiscordAdapter {
    fn from(cfg: DiscordConfig) -> Self {
        let http = Arc::new(Http::new(&cfg.bot_token));
        Self {
            bot_token: cfg.bot_token,
            http,
            self_id: Arc::new(OnceLock::new()),
            typing: TypingIndicator::new(),
            splitter: super::split::MarkdownSplitter::new(DISCORD_MAX_MESSAGE_LEN, None),
        }
    }
}

#[async_trait]
impl ChannelAdapter for DiscordAdapter {
    async fn on_connect(&self, ctx: &ChannelCtx) -> Result<(), AppError> {
        let me = self.http.get_current_user().await.map_err(|e| {
            tracing::warn!(
                channel_id = %ctx.channel.id,
                error = %e,
                "Discord get_current_user failed — bot_token rejected",
            );
            AppError::Validation(format!("Discord rejected the bot_token: {e}"))
        })?;
        let _ = self.self_id.set(me.id);

        tracing::info!(
            channel_id = %ctx.channel.id,
            discord_user_id = %me.id,
            username = %me.name,
            "Discord bot authenticated",
        );

        let handler = DiscordEventHandler {
            emit: ctx.emit.clone(),
            channel_id_log: ctx.channel.id.clone(),
            self_id: self.self_id.clone(),
            hitl: ctx.hitl.clone(),
            chat_service: ctx.chat_service.clone(),
        };
        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;
        let mut client = Client::builder(&self.bot_token, intents)
            .event_handler(handler)
            .await
            .map_err(|e| AppError::Internal(format!("Discord client build failed: {e}")))?;
        let shard_manager = client.shard_manager.clone();

        let cancel = ctx.cancel.clone();
        let channel_id = ctx.channel.id.clone();
        let signals = ctx.signals.clone();
        tokio::spawn(async move {
            let outcome = tokio::select! {
                res = client.start() => GatewayOutcome::Stopped(res),
                _ = cancel.cancelled() => GatewayOutcome::Cancelled,
            };
            match outcome {
                GatewayOutcome::Stopped(Err(e)) => {
                    tracing::warn!(channel_id = %channel_id, error = %e, "Discord gateway terminated");
                    signals.disconnected_transient(format!("Discord gateway failed: {e}"));
                }
                GatewayOutcome::Stopped(Ok(())) => {
                    tracing::info!(channel_id = %channel_id, "Discord gateway stopped cleanly");
                }
                GatewayOutcome::Cancelled => {
                    shard_manager.shutdown_all().await;
                    tracing::info!(
                        channel_id = %channel_id,
                        "Discord gateway shut down (channel cancelled)",
                    );
                }
            }
        });

        // Auth succeeded above; the gateway is coming up — report readiness.
        ctx.signals.connected();
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
        _ctx: &ChannelCtx,
    ) -> Result<(), ChannelError> {
        let Some(text) = tool_call.turn_text.as_deref() else {
            return Ok(());
        };
        if text.trim().is_empty() {
            return Ok(());
        }
        self.post_message(chat, text).await
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

        // Fast path: pre-existing text-only behavior.
        if !has_attachments {
            if body.trim().is_empty() {
                return Ok(());
            }
            return self.post_message(chat, &body).await;
        }

        // Discord supports content + files + components in a single message.
        // We do one bubble per chunk if body exceeds DISCORD_MAX_MESSAGE_LEN.
        let channel_id = parse_external_id(external_chat_id(chat)?)?;

        // Build attachment payloads.
        let mut files: Vec<CreateAttachment> = Vec::new();
        let mut buttons: Vec<CreateButton> = Vec::new();
        for att in &msg.attachments {
            let kind = attachment::classify(att);
            if attachment::is_media(kind) {
                match attachment::read_attachment_bytes(att, ctx).await {
                    Ok(bytes) => {
                        files.push(CreateAttachment::bytes(bytes, att.filename.clone()));
                    }
                    Err(e) => {
                        tracing::warn!(
                            msg_id = %msg.id,
                            path = %att.path,
                            error = %e,
                            "discord: failed to read media bytes; skipping",
                        );
                    }
                }
            } else {
                let url = match attachment::outbound_url(att, ctx, attachment::ChannelMode::Button)
                    .await
                {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(
                            msg_id = %msg.id,
                            path = %att.path,
                            error = %e,
                            "discord: canonical URL failed; skipping doc",
                        );
                        continue;
                    }
                };
                let label = attachment::button_label(att);
                buttons.push(CreateButton::new_link(url).label(label));
            }
        }

        // Discord cap: 5 rows × 5 = 25 buttons max. Truncate beyond that.
        const MAX_BUTTONS: usize = 25;
        let truncated_count = buttons.len().saturating_sub(MAX_BUTTONS);
        buttons.truncate(MAX_BUTTONS);
        let action_rows: Vec<CreateActionRow> = buttons
            .chunks(5)
            .map(|chunk| CreateActionRow::Buttons(chunk.to_vec()))
            .collect();

        let mut body_with_overflow = body.clone();
        if truncated_count > 0 {
            tracing::warn!(
                msg_id = %msg.id,
                truncated = truncated_count,
                "discord: too many doc buttons; truncated",
            );
            body_with_overflow.push_str(&format!(
                "\n\n(plus {} more attachment{} — see in app)",
                truncated_count,
                if truncated_count == 1 { "" } else { "s" }
            ));
        }

        // Discord requires non-empty content if there are no embeds/files.
        // Our message always has files OR components when there are attachments
        // (we only reach this branch when `has_attachments`), so empty body
        // is fine.
        let mut req = CreateMessage::new();
        if !body_with_overflow.is_empty() {
            // Discord caps content at 2000; chunk if needed.
            // For attachment messages we keep the first chunk (the rest goes
            // before the attachments as separate text messages).
            let fenced = super::markdown::fence_tables(&body_with_overflow);
            let chunks = self.splitter.split(&fenced);
            let mut iter = chunks.into_iter();
            if let Some(first) = iter.next() {
                req = req.content(first);
            }
            for extra in iter {
                let _ = channel_id
                    .send_message(&*self.http, CreateMessage::new().content(extra))
                    .await;
            }
        }
        if !files.is_empty() {
            req = req.add_files(files);
        }
        if !action_rows.is_empty() {
            req = req.components(action_rows);
        }

        if let Err(e) = channel_id.send_message(&*self.http, req).await {
            return Err(classify_discord_error(&e));
        }
        Ok(())
    }

    async fn on_inference_start(&self, chat: &Chat, _ctx: &ChannelCtx) -> Result<(), ChannelError> {
        let Ok(external_id) = external_chat_id(chat) else {
            return Ok(());
        };
        let Ok(discord_channel_id) = parse_external_id(external_id) else {
            return Ok(());
        };

        let http = self.http.clone();
        self.typing
            .start(chat.id.clone(), TYPING_REFRESH_INTERVAL, move || {
                let http = http.clone();
                async move {
                    if let Err(e) = discord_channel_id.broadcast_typing(&*http).await {
                        tracing::debug!(
                            channel_id = %discord_channel_id,
                            error = %e,
                            "Discord broadcast_typing failed (best-effort)",
                        );
                    }
                }
            })
            .await;
        Ok(())
    }

    async fn on_inference_done(&self, chat: &Chat, _ctx: &ChannelCtx) -> Result<(), ChannelError> {
        self.typing.stop(&chat.id).await;
        Ok(())
    }

    async fn on_pending_hitl(
        &self,
        batch: &[crate::inference::tool_call::ToolCall],
        _msg: &Message,
        chat: &Chat,
        ctx: &ChannelCtx,
    ) -> Result<Vec<crate::inference::hitl::HitlDelivery>, ChannelError> {
        let channel_id = parse_external_id(external_chat_id(chat)?)?;
        let mut out = Vec::with_capacity(batch.len());
        for tc in batch {
            let Some(h) = tc.hitl.as_ref() else { continue };
            let kind = crate::chat::channel::hitl::kind_for(&h.request);
            let body = super::markdown::fence_tables(&h.prompt).into_owned();
            let components = build_discord_components(&tc.id, &kind, &h.url);
            let req = CreateMessage::new().content(body).components(components);
            match channel_id.send_message(&*self.http, req).await {
                Ok(sent) => out.push(crate::inference::hitl::HitlDelivery {
                    channel_id: ctx.channel.id.clone(),
                    external_message_id: sent.id.get().to_string(),
                    delivered_at: chrono::Utc::now(),
                }),
                Err(e) => {
                    let classified = classify_discord_error(&e);
                    tracing::warn!(
                        channel_id = %ctx.channel.id,
                        tool_call_id = %tc.id,
                        kind = ?classified.kind,
                        error = %e,
                        "Discord on_pending_hitl: send failed",
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

fn classify_discord_error(err: &SerenityError) -> ChannelError {
    let msg = err.to_string();
    match err {
        SerenityError::Http(HttpError::UnsuccessfulRequest(resp)) => {
            let code = resp.status_code.as_u16();
            match code {
                429 => ChannelError::transient(msg),
                401 => ChannelError::terminal(msg, ChannelErrorKind::Unauthorized),
                403 => ChannelError::terminal(msg, ChannelErrorKind::Forbidden),
                404 => ChannelError::terminal(msg, ChannelErrorKind::NotFound),
                413 => ChannelError::terminal(msg, ChannelErrorKind::PayloadTooLarge),
                400 => ChannelError::terminal(msg, ChannelErrorKind::PayloadInvalid),
                500..=599 => ChannelError::transient(msg),
                _ => ChannelError::terminal(msg, ChannelErrorKind::Other),
            }
        }
        // Non-HTTP errors are gateway/network: transient by default.
        _ => ChannelError::transient(msg),
    }
}

impl DiscordAdapter {
    async fn post_message(&self, chat: &Chat, text: &str) -> Result<(), ChannelError> {
        let channel_id = parse_external_id(external_chat_id(chat)?)?;
        let fenced = super::markdown::fence_tables(text);
        for chunk in self.splitter.split(&fenced) {
            let req = CreateMessage::new().content(chunk);
            channel_id
                .send_message(&*self.http, req)
                .await
                .map_err(|e| classify_discord_error(&e))?;
        }
        Ok(())
    }
}

enum GatewayOutcome {
    Stopped(Result<(), SerenityError>),
    Cancelled,
}

struct DiscordEventHandler {
    emit: tokio::sync::mpsc::Sender<ExternalMessage>,
    channel_id_log: String,
    self_id: Arc<OnceLock<UserId>>,
    hitl: Arc<super::super::HitlDeliveryService>,
    chat_service: crate::chat::service::ChatService,
}

#[async_trait]
impl EventHandler for DiscordEventHandler {
    async fn message(&self, _ctx: Context, msg: DiscordMessage) {
        let Some(&self_id) = self.self_id.get() else {
            return;
        };
        if let Some(em) = convert_message(&msg, self_id)
            && let Err(e) = self.emit.send(em).await
        {
            tracing::warn!(
                channel_id = %self.channel_id_log,
                error = %e,
                "Discord inbound emit channel closed",
            );
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Component(component) = interaction else {
            return;
        };
        let custom_id = component.data.custom_id.clone();

        let parsed =
            crate::chat::channel::hitl::parse_resolve_callback_data(&custom_id, &self.chat_service)
                .await;

        let (tool_call_id, response) = match parsed {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    channel_id = %self.channel_id_log,
                    data = %custom_id,
                    error = %e,
                    "Discord callback_data parse failed",
                );
                let resp = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .content("Could not interpret that action."),
                );
                let _ = component.create_response(&ctx.http, resp).await;
                return;
            }
        };

        let answer_label = crate::chat::channel::hitl::response_display(&response);
        let outcome = self.hitl.resolve(&tool_call_id, response).await;

        let summary = match &outcome {
            Ok(crate::inference::hitl::ResolveOutcome::Resolved { .. }) => answer_label,
            Ok(crate::inference::hitl::ResolveOutcome::AlreadyResolved) => {
                "Already resolved".to_string()
            }
            Err(e) => format!("Failed: {e}"),
        };

        let original = component.message.content.clone();
        let new_content = format!("{original}\n\n→ {summary}");
        let resp = CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(new_content)
                .components(Vec::new()),
        );
        if let Err(e) = component.create_response(&ctx.http, resp).await {
            tracing::warn!(
                channel_id = %self.channel_id_log,
                error = %e,
                "Discord interaction response failed",
            );
        }
    }
}

const DISCORD_LABEL_MAX: usize = 80;
const DISCORD_CHOICE_BUTTON_CAP: usize = 5;

fn truncate_label(s: &str) -> String {
    if s.chars().count() <= DISCORD_LABEL_MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(DISCORD_LABEL_MAX - 1).collect();
        out.push('…');
        out
    }
}

/// Mirror of `telegram::build_inline_keyboard`. Approval → Yes/No row plus a
/// URL fallback (the only Approval path today is `manage_app` deploy, where
/// the user needs the link to inspect the manifest). Choice → option buttons
/// only — they ARE the resolve action. External → URL only. Discord rejects
/// link buttons with non-http(s) URLs, so URL rows silently drop if invalid.
fn build_discord_components(
    tcid: &str,
    kind: &crate::chat::channel::hitl::HitlKind,
    url: &str,
) -> Vec<CreateActionRow> {
    use crate::chat::channel::hitl::HitlKind;
    let url_row = || -> Option<CreateActionRow> {
        if url.starts_with("http://") || url.starts_with("https://") {
            Some(CreateActionRow::Buttons(vec![
                CreateButton::new_link(url).label("Open on web →"),
            ]))
        } else {
            None
        }
    };
    match kind {
        HitlKind::Approval => {
            let mut rows = vec![CreateActionRow::Buttons(vec![
                CreateButton::new(format!("r:{tcid}:y"))
                    .label("Yes")
                    .style(ButtonStyle::Success),
                CreateButton::new(format!("r:{tcid}:n"))
                    .label("No")
                    .style(ButtonStyle::Danger),
            ])];
            if let Some(row) = url_row() {
                rows.push(row);
            }
            rows
        }
        HitlKind::Choice { options } => options
            .iter()
            .take(DISCORD_CHOICE_BUTTON_CAP)
            .enumerate()
            .map(|(i, opt)| {
                CreateActionRow::Buttons(vec![
                    CreateButton::new(format!("r:{tcid}:c:{i}"))
                        .label(truncate_label(opt))
                        .style(ButtonStyle::Primary),
                ])
            })
            .collect(),
        HitlKind::External => url_row().map(|r| vec![r]).unwrap_or_default(),
    }
}

fn convert_message(msg: &DiscordMessage, self_id: UserId) -> Option<ExternalMessage> {
    if should_skip(msg.author.id, msg.author.bot, &msg.content, self_id) {
        return None;
    }
    let display = msg
        .member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .or_else(|| msg.author.global_name.clone())
        .or_else(|| Some(msg.author.name.clone()));
    Some(ExternalMessage {
        external_chat_id: build_external_chat_id(msg.channel_id, msg.guild_id.is_none()),
        sender_address: msg.author.id.to_string(),
        sender_external_id: Some(msg.author.id.to_string()),
        sender_display_name: display,
        content: msg.content.clone(),
        attachments: Vec::new(),
    })
}

fn should_skip(author_id: UserId, author_bot: bool, content: &str, self_id: UserId) -> bool {
    author_id == self_id || author_bot || content.trim().is_empty()
}

fn build_external_chat_id(channel_id: ChannelId, is_dm: bool) -> String {
    if is_dm {
        format!("dm:{channel_id}")
    } else {
        format!("group:{channel_id}")
    }
}

fn parse_external_id(s: &str) -> Result<ChannelId, AppError> {
    let (kind, id_str) = s.split_once(':').ok_or_else(|| {
        AppError::Validation(format!("unrecognised Discord external_id format: {s:?}"))
    })?;
    if !matches!(kind, "dm" | "group") || id_str.is_empty() {
        return Err(AppError::Validation(format!(
            "unrecognised Discord external_id format: {s:?}"
        )));
    }
    let id: u64 = id_str
        .parse()
        .map_err(|_| AppError::Validation(format!("invalid Discord channel id: {id_str}")))?;
    Ok(ChannelId::new(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Tests that construct `DiscordAdapter::from(...)` must call this: rustls
    /// panics without an installed `CryptoProvider`, and tests don't go
    /// through server startup where production installs it.
    fn install_crypto_provider() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            crate::initialize_tls();
        });
    }

    #[test]
    fn manifest_declares_required_secret_bot_token() {
        let m = DiscordAdapterFactory.manifest();
        assert_eq!(m.id, "discord");
        assert_eq!(m.display_name, "Discord");
        let f = m
            .config_fields
            .iter()
            .find(|f| f.name == "bot_token")
            .expect("bot_token field missing");
        assert!(f.is_required);
        assert!(f.is_secret);
    }

    #[test]
    fn factory_create_with_valid_config_succeeds() {
        install_crypto_provider();
        let cfg = json!({"bot_token": "abc.def.ghi"});
        DiscordAdapterFactory
            .create(cfg)
            .expect("valid config should produce a DiscordAdapter");
    }

    #[test]
    fn factory_create_rejects_missing_bot_token() {
        let cfg = json!({});
        assert!(matches!(
            DiscordAdapterFactory.create(cfg),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn parse_external_id_dm() {
        let c = parse_external_id("dm:123456789012345678").unwrap();
        assert_eq!(c.get(), 123456789012345678);
    }

    #[test]
    fn parse_external_id_group() {
        let c = parse_external_id("group:987654321098765432").unwrap();
        assert_eq!(c.get(), 987654321098765432);
    }

    #[test]
    fn parse_external_id_rejects_garbage() {
        assert!(parse_external_id("nonsense").is_err());
        assert!(parse_external_id("dm:").is_err());
        assert!(parse_external_id("group:").is_err());
        assert!(parse_external_id("group:not-a-number").is_err());
        assert!(parse_external_id("thread:123").is_err());
    }

    #[test]
    fn build_external_chat_id_dm() {
        assert_eq!(build_external_chat_id(ChannelId::new(42), true), "dm:42",);
    }

    #[test]
    fn build_external_chat_id_group() {
        assert_eq!(
            build_external_chat_id(ChannelId::new(999), false),
            "group:999",
        );
    }

    #[test]
    fn should_skip_self_message() {
        let me = UserId::new(1);
        assert!(should_skip(me, false, "hi", me));
    }

    #[test]
    fn should_skip_bot_author() {
        assert!(should_skip(UserId::new(2), true, "hi", UserId::new(1)));
    }

    #[test]
    fn should_skip_empty_content() {
        assert!(should_skip(UserId::new(2), false, "   ", UserId::new(1)));
    }

    #[test]
    fn should_not_skip_human_message() {
        assert!(!should_skip(UserId::new(2), false, "hello", UserId::new(1)));
    }
}
