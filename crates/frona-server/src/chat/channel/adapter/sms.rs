use async_trait::async_trait;
use axum::body::Bytes;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha1::Sha1;

use crate::chat::channel::CarrierStatus;
use crate::chat::channel::adapter::markdown;
use crate::chat::message::models::Message;
use crate::chat::models::Chat;
use crate::core::error::AppError;

use super::super::attachment;
use super::super::error::{ChannelError, ChannelErrorKind};
use super::super::models::{ChannelAdapter, ChannelCtx, ExternalMessage, external_chat_id};
#[cfg(test)]
use super::super::models::{ChannelFactory, ConfigRef};

const TWIML_EMPTY_RESPONSE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response/>";
const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01";

/// Twilio Programmable Messaging hard cap (concatenated SMS, segmented at
/// the carrier). Used as both `provider_limit` and `hard_limit` — SMS
/// triggers truncate-with-link mode for replies exceeding the cap.
const SMS_MAX_MESSAGE_LEN: usize = 1600;

#[derive(Debug, Clone, Deserialize)]
pub struct SmsConfig {
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
}

#[derive(crate::ChannelFactory)]
#[channel(id = "sms", from = SmsConfig)]
pub struct SmsAdapter {
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    splitter: super::split::PlainSplitter,
}

impl From<SmsConfig> for SmsAdapter {
    fn from(cfg: SmsConfig) -> Self {
        Self {
            account_sid: cfg.account_sid,
            auth_token: cfg.auth_token,
            from_number: cfg.from_number,
            splitter: super::split::PlainSplitter::new(
                SMS_MAX_MESSAGE_LEN,
                Some(SMS_MAX_MESSAGE_LEN),
            ),
        }
    }
}

impl SmsAdapter {
    fn twilio(&self) -> TwilioApi<'_> {
        TwilioApi {
            account_sid: &self.account_sid,
            auth_token: &self.auth_token,
        }
    }
}

#[async_trait]
impl ChannelAdapter for SmsAdapter {
    async fn on_connect(&self, ctx: &ChannelCtx) -> Result<(), AppError> {
        let sid = self
            .twilio()
            .register_messaging_webhook(&self.from_number, &ctx.webhook_url)
            .await
            .map_err(|e| {
                tracing::warn!(
                    channel_id = %ctx.channel.id,
                    from_number = %self.from_number,
                    url = %ctx.webhook_url,
                    error = %e,
                    "SMS channel could not auto-register Twilio webhook — channel will be marked Failed (fix the underlying issue and restart, or paste the URL into the Twilio console manually)",
                );
                e
            })?;
        tracing::info!(
            channel_id = %ctx.channel.id,
            phone_number_sid = %sid,
            from_number = %self.from_number,
            url = %ctx.webhook_url,
            "SMS channel registered Twilio Messaging webhook",
        );
        ctx.signals.connected();
        Ok(())
    }

    async fn on_disconnect(&self, _ctx: &ChannelCtx) -> Result<(), AppError> {
        Ok(())
    }

    async fn on_send(
        &self,
        msg: &Message,
        tool_calls: &[crate::inference::tool_call::ToolCall],
        chat: &Chat,
        ctx: &ChannelCtx,
    ) -> Result<(), ChannelError> {
        let raw_body = crate::chat::channel::render::render_message_body(msg);
        let mut body = compose_sms_body(tool_calls, &raw_body);

        // Inline short links for any attachments. No emoji to save char
        // budget in SMS segments.
        for att in &msg.attachments {
            match attachment::outbound_url(att, ctx, attachment::ChannelMode::Inline).await {
                Ok(url) => {
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str(&format!("{} — {url}", att.filename));
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %ctx.channel.id,
                        msg_id = %msg.id,
                        path = %att.path,
                        error = %e,
                        "SMS: share_url issue failed; skipping attachment",
                    );
                }
            }
        }

        if body.trim().is_empty() {
            return Ok(());
        }

        let to_number = parse_external_id(external_chat_id(chat)?)?;
        let status_callback = status_callback_url(&ctx.webhook_url, &msg.id);

        let sctx = super::split::SplitCtx {
            chat_id: &chat.id,
            user_id: &ctx.channel.user_id,
        };
        let chunks = self.splitter.split(&body, ctx, sctx).await?;

        tracing::info!(
            channel_id = %ctx.channel.id,
            msg_id = %msg.id,
            from = %self.from_number,
            to = %to_number,
            content_len = body.len(),
            chunks = chunks.len(),
            tool_count = tool_calls.len(),
            "SMS on_send: dispatching split body to Twilio",
        );
        for chunk in chunks {
            self.twilio()
                .send_message(&self.from_number, &to_number, &chunk, &status_callback)
                .await
                .map_err(|e| {
                    tracing::warn!(
                        channel_id = %ctx.channel.id,
                        msg_id = %msg.id,
                        to = %to_number,
                        error = %e,
                        "SMS on_send: Twilio synchronously rejected message",
                    );
                    e
                })?;
        }
        tracing::debug!(
            channel_id = %ctx.channel.id,
            msg_id = %msg.id,
            to = %to_number,
            "SMS on_send: Twilio accepted message (carrier delivery status will arrive via webhook)",
        );
        Ok(())
    }

    async fn on_pending_hitl(
        &self,
        batch: &[crate::inference::tool_call::ToolCall],
        _msg: &Message,
        chat: &Chat,
        ctx: &ChannelCtx,
    ) -> Result<Vec<crate::inference::hitl::HitlDelivery>, ChannelError> {
        // SMS is text-only → sequential cadence: render only the first pending
        // HITL. The delivery cursor advances by 1; the next pending HITL
        // renders after this one resolves.
        let Some(tc) = batch.first() else {
            return Ok(Vec::new());
        };
        let Some(h) = tc.hitl.as_ref() else {
            return Ok(Vec::new());
        };

        let body = crate::chat::channel::hitl::render_text(h);
        let to_number = parse_external_id(external_chat_id(chat)?)?;
        let status_callback = status_callback_url(&ctx.webhook_url, &tc.id);

        let sctx = super::split::SplitCtx {
            chat_id: &chat.id,
            user_id: &ctx.channel.user_id,
        };
        let chunks = self.splitter.split(&body, ctx, sctx).await?;
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let mut last_sid = String::new();
        for chunk in chunks {
            match self
                .twilio()
                .send_message(&self.from_number, &to_number, &chunk, &status_callback)
                .await
            {
                Ok(sid) => last_sid = sid,
                Err(e) => {
                    tracing::warn!(
                        channel_id = %ctx.channel.id,
                        tool_call_id = %tc.id,
                        error = %e,
                        "SMS on_pending_hitl: send failed",
                    );
                    return Ok(Vec::new());
                }
            }
        }

        Ok(vec![crate::inference::hitl::HitlDelivery {
            channel_id: ctx.channel.id.clone(),
            external_message_id: last_sid,
            delivered_at: chrono::Utc::now(),
        }])
    }

    async fn on_webhook(
        &self,
        ctx: &ChannelCtx,
        request: Request<Bytes>,
    ) -> Result<Response, ChannelError> {
        let Some(signature) = header_str(&request, "X-Twilio-Signature") else {
            return Ok(forbidden("missing X-Twilio-Signature"));
        };
        let canonical_url = canonical_webhook_url(&ctx.webhook_url, request.uri().query());
        let raw_params = parse_form_body(request.body());
        if !verify_twilio_signature(&self.auth_token, &canonical_url, &raw_params, &signature) {
            return Ok(forbidden("Twilio signature mismatch"));
        }

        let webhook = TwilioWebhook::from_pairs(&raw_params);
        if webhook.is_status_callback() {
            webhook
                .apply_carrier_status(ctx, request.uri().query())
                .await;
            return Ok(ok_twiml());
        }
        if webhook.body.is_empty() && webhook.num_media == "0" {
            return Ok(ok_twiml());
        }
        if webhook.from.is_empty() {
            return Err(AppError::Validation("Twilio webhook missing From".into()).into());
        }
        webhook.emit_inbound(ctx).await?;
        Ok(ok_twiml())
    }
}

struct TwilioApi<'a> {
    account_sid: &'a str,
    auth_token: &'a str,
}

impl TwilioApi<'_> {
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.basic_auth(self.account_sid, Some(self.auth_token))
    }

    fn url(&self, path: &str) -> String {
        format!("{TWILIO_API_BASE}/Accounts/{}/{path}", self.account_sid)
    }

    async fn send_message(
        &self,
        from: &str,
        to: &str,
        body: &str,
        status_callback: &str,
    ) -> Result<String, ChannelError> {
        #[derive(Deserialize)]
        struct Out {
            sid: String,
        }
        let resp = self
            .auth(crate::build_http_client().post(self.url("Messages.json")))
            .form(&[
                ("From", from),
                ("To", to),
                ("Body", body),
                ("StatusCallback", status_callback),
            ])
            .send()
            .await
            .map_err(|e| {
                classify_twilio_error(
                    &TwilioError::Transport,
                    format!("Twilio send Messages: {e}"),
                )
            })?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            let msg = format!("Twilio send Messages HTTP {status}: {body}");
            return Err(classify_twilio_error(
                &TwilioError::from_http(status, &body),
                msg,
            ));
        }
        let parsed: Out = resp.json().await.map_err(|e| {
            classify_twilio_error(
                &TwilioError::Transport,
                format!("Twilio send Messages parse: {e}"),
            )
        })?;
        Ok(parsed.sid)
    }

    async fn register_messaging_webhook(
        &self,
        phone_number: &str,
        webhook_url: &str,
    ) -> Result<String, AppError> {
        #[derive(Deserialize)]
        struct PhoneNumber {
            sid: String,
        }
        #[derive(Deserialize)]
        struct ListResponse {
            incoming_phone_numbers: Vec<PhoneNumber>,
        }

        let client = crate::build_http_client();
        let list: ListResponse = ok_json(
            self.auth(client.get(self.url("IncomingPhoneNumbers.json")))
                .query(&[("PhoneNumber", phone_number)])
                .send()
                .await
                .map_err(|e| {
                    AppError::Internal(format!("Twilio list IncomingPhoneNumbers: {e}"))
                })?,
            "list IncomingPhoneNumbers",
        )
        .await?;
        let sid = list
            .incoming_phone_numbers
            .into_iter()
            .next()
            .map(|n| n.sid)
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "Twilio account does not own {phone_number} — buy or port the number, or correct the from_number config",
                ))
            })?;

        ok_empty(
            self.auth(client.post(self.url(&format!("IncomingPhoneNumbers/{sid}.json"))))
                .form(&[("SmsUrl", webhook_url), ("SmsMethod", "POST")])
                .send()
                .await
                .map_err(|e| {
                    AppError::Internal(format!("Twilio update IncomingPhoneNumber: {e}"))
                })?,
            "update IncomingPhoneNumber",
        )
        .await?;
        Ok(sid)
    }
}

async fn ok_json<T: for<'de> Deserialize<'de>>(
    resp: reqwest::Response,
    op: &str,
) -> Result<T, AppError> {
    let resp = ensure_status(resp, op).await?;
    resp.json()
        .await
        .map_err(|e| AppError::Internal(format!("Twilio {op} parse: {e}")))
}

async fn ok_empty(resp: reqwest::Response, op: &str) -> Result<(), AppError> {
    ensure_status(resp, op).await.map(|_| ())
}

/// See https://www.twilio.com/docs/api/errors
#[derive(Debug, Clone)]
enum TwilioError {
    Transport,
    HttpStatusOnly(u16),
    /// 20003
    AuthError,
    /// 20404
    NotFound,
    /// 20429
    RateLimit,
    /// 21211
    InvalidToNumber,
    /// 21212
    InvalidFromNumber,
    /// 21408 — region not in Geo Permissions allow-list.
    GeoPermissionDenied,
    /// 21601
    NumberNotSmsCapable,
    /// 21610 — recipient previously replied STOP.
    RecipientOptedOut,
    /// 21611
    MessageTooLong,
    /// 21612
    ChannelMismatch,
    /// 21617
    SegmentLimitExceeded,
    /// 21703
    MessagingServiceUnusable,
    /// 30003 — phone might come back online, retry.
    HandsetUnreachable,
    /// 30004
    MessageBlocked,
    /// 30005
    UnknownDestination,
    /// 30006
    LandlineOrUnreachable,
    /// 30007
    CarrierFiltered,
    /// 30008 — opaque from carrier, retry.
    UnknownCarrierError,
    Unknown,
}

#[derive(Debug, Deserialize)]
struct TwilioErrorBody {
    code: Option<u32>,
}

impl TwilioError {
    fn from_http(status: u16, body: &str) -> Self {
        if let Ok(parsed) = serde_json::from_str::<TwilioErrorBody>(body)
            && let Some(code) = parsed.code
        {
            return Self::from_code(code);
        }
        Self::HttpStatusOnly(status)
    }

    fn from_code(code: u32) -> Self {
        match code {
            20003 => Self::AuthError,
            20404 => Self::NotFound,
            20429 => Self::RateLimit,
            21211 => Self::InvalidToNumber,
            21212 => Self::InvalidFromNumber,
            21408 => Self::GeoPermissionDenied,
            21601 => Self::NumberNotSmsCapable,
            21610 => Self::RecipientOptedOut,
            21611 => Self::MessageTooLong,
            21612 => Self::ChannelMismatch,
            21617 => Self::SegmentLimitExceeded,
            21703 => Self::MessagingServiceUnusable,
            30003 => Self::HandsetUnreachable,
            30004 => Self::MessageBlocked,
            30005 => Self::UnknownDestination,
            30006 => Self::LandlineOrUnreachable,
            30007 => Self::CarrierFiltered,
            30008 => Self::UnknownCarrierError,
            _ => Self::Unknown,
        }
    }

    fn to_channel_error(&self, msg: String) -> ChannelError {
        use ChannelErrorKind::*;
        match self {
            Self::Transport
            | Self::Unknown
            | Self::RateLimit
            | Self::HandsetUnreachable
            | Self::UnknownCarrierError => ChannelError::transient(msg),
            Self::HttpStatusOnly(status) => match status {
                401 => ChannelError::terminal(msg, Unauthorized),
                403 => ChannelError::terminal(msg, Forbidden),
                404 => ChannelError::terminal(msg, NotFound),
                413 => ChannelError::terminal(msg, PayloadTooLarge),
                429 => ChannelError::transient(msg),
                500..=599 => ChannelError::transient(msg),
                400 | 422 => ChannelError::terminal(msg, PayloadInvalid),
                _ => ChannelError::transient(msg),
            },
            Self::AuthError => ChannelError::terminal(msg, Unauthorized),
            Self::GeoPermissionDenied
            | Self::RecipientOptedOut
            | Self::MessageBlocked
            | Self::CarrierFiltered
            | Self::MessagingServiceUnusable => ChannelError::terminal(msg, Forbidden),
            Self::NotFound
            | Self::UnknownDestination
            | Self::LandlineOrUnreachable
            | Self::NumberNotSmsCapable => ChannelError::terminal(msg, NotFound),
            Self::MessageTooLong | Self::SegmentLimitExceeded => {
                ChannelError::terminal(msg, PayloadTooLarge)
            }
            Self::InvalidToNumber | Self::InvalidFromNumber | Self::ChannelMismatch => {
                ChannelError::terminal(msg, PayloadInvalid)
            }
        }
    }
}

fn classify_twilio_error(err: &TwilioError, msg: String) -> ChannelError {
    err.to_channel_error(msg)
}

async fn ensure_status(resp: reqwest::Response, op: &str) -> Result<reqwest::Response, AppError> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(AppError::Internal(format!(
            "Twilio {op} HTTP {status}: {body}"
        )))
    }
}

#[derive(Debug, Default)]
struct TwilioWebhook {
    from: String,
    body: String,
    message_sid: String,
    num_media: String,
    message_status: String,
    error_code: String,
    error_message: String,
}

impl TwilioWebhook {
    fn from_pairs(pairs: &[(String, String)]) -> Self {
        let mut p = Self {
            num_media: "0".into(),
            ..Self::default()
        };
        for (k, v) in pairs {
            match k.as_str() {
                "From" => p.from = v.clone(),
                "Body" => p.body = v.clone(),
                "MessageSid" => p.message_sid = v.clone(),
                "NumMedia" => p.num_media = v.clone(),
                "MessageStatus" => p.message_status = v.clone(),
                "ErrorCode" => p.error_code = v.clone(),
                "ErrorMessage" => p.error_message = v.clone(),
                _ => {}
            }
        }
        p
    }

    // Twilio inbound also sets MessageStatus, so discriminate on body/media.
    fn is_status_callback(&self) -> bool {
        !self.message_status.is_empty() && self.body.is_empty() && self.num_media == "0"
    }

    async fn apply_carrier_status(&self, ctx: &ChannelCtx, query: Option<&str>) {
        let our_msg_id = msg_id_from_query(query);
        match self.message_status.as_str() {
            "delivered" => {
                if let Some(id) = our_msg_id.as_deref() {
                    let _ = ctx
                        .outbound
                        .record_carrier_status(id, CarrierStatus::Delivered)
                        .await;
                }
                tracing::debug!(
                    channel_id = %ctx.channel.id,
                    twilio_sid = %self.message_sid,
                    msg_id = ?our_msg_id,
                    "SMS delivery status: delivered",
                );
            }
            "failed" | "undelivered" => {
                let error = self.format_carrier_error();
                if let Some(id) = our_msg_id.as_deref() {
                    let _ = ctx
                        .outbound
                        .record_carrier_status(
                            id,
                            CarrierStatus::Failed {
                                error: error.clone(),
                            },
                        )
                        .await;
                }
                tracing::warn!(
                    channel_id = %ctx.channel.id,
                    twilio_sid = %self.message_sid,
                    msg_id = ?our_msg_id,
                    status = %self.message_status,
                    error_code = %self.error_code,
                    error_message = %self.error_message,
                    "SMS delivery failed at carrier (e.g. unregistered 10DLC, blocked recipient, invalid number)",
                );
            }
            other => tracing::debug!(
                channel_id = %ctx.channel.id,
                twilio_sid = %self.message_sid,
                status = %other,
                "SMS delivery status update",
            ),
        }
    }

    async fn emit_inbound(&self, ctx: &ChannelCtx) -> Result<(), AppError> {
        tracing::debug!(
            channel_id = %ctx.channel.id,
            from = %self.from,
            message_sid = %self.message_sid,
            "SMS webhook accepted — emitting to inbound pipeline",
        );
        let event = ExternalMessage {
            external_chat_id: format!("sms:{}", self.from),
            sender_address: self.from.clone(),
            sender_external_id: Some(self.from.clone()),
            sender_display_name: Some(self.from.clone()),
            content: self.body.clone(),
            attachments: vec![],
        };
        ctx.emit
            .send(event)
            .await
            .map_err(|e| AppError::Internal(format!("inbound emit channel closed: {e}")))
    }

    fn format_carrier_error(&self) -> String {
        if self.error_message.is_empty() {
            format!("Twilio {} ({})", self.message_status, self.error_code)
        } else {
            format!(
                "{} ({} {})",
                self.error_message, self.message_status, self.error_code
            )
        }
    }
}

fn parse_form_body(body: &[u8]) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> =
        url::form_urlencoded::parse(body).into_owned().collect();
    params.sort_by(|a, b| a.0.cmp(&b.0));
    params
}

fn header_str<B>(request: &Request<B>, name: &str) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn msg_id_from_query(query: Option<&str>) -> Option<String> {
    let q = query?;
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == "msg_id")
        .map(|(_, v)| v.into_owned())
}

// Twilio echoes the query back, letting carrier-status skip a DB lookup.
fn status_callback_url(webhook_url: &str, msg_id: &str) -> String {
    let encoded: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("msg_id", msg_id)
        .finish();
    format!("{webhook_url}?{encoded}")
}

fn canonical_webhook_url(webhook_url: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("{webhook_url}?{q}"),
        _ => webhook_url.to_string(),
    }
}

// HMAC-SHA1 of url + sorted(k+v).concat(), base64; constant-time compare.
fn verify_twilio_signature(
    auth_token: &str,
    url: &str,
    params: &[(String, String)],
    provided_b64: &str,
) -> bool {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut canonical = String::with_capacity(url.len());
    canonical.push_str(url);
    for (k, v) in &sorted {
        canonical.push_str(k);
        canonical.push_str(v);
    }

    let mut mac = match Hmac::<Sha1>::new_from_slice(auth_token.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(canonical.as_bytes());
    let expected = mac.finalize().into_bytes();

    let provided = match base64::engine::general_purpose::STANDARD.decode(provided_b64) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };

    if provided.len() != expected.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (a, b) in expected.iter().zip(provided.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn parse_external_id(s: &str) -> Result<String, AppError> {
    match s.strip_prefix("sms:") {
        Some(rest) if !rest.is_empty() => Ok(rest.to_string()),
        _ => Err(AppError::Validation(format!(
            "unrecognised SMS external_id format: {s:?}"
        ))),
    }
}

fn compose_sms_body(
    tool_calls: &[crate::inference::tool_call::ToolCall],
    trailing: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for tc in tool_calls {
        if let Some(text) = tc.turn_text.as_deref() {
            let plain = markdown::to_plain(text);
            let trimmed = plain.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    let trailing_plain = markdown::to_plain(trailing);
    let trailing_trimmed = trailing_plain.trim();
    if !trailing_trimmed.is_empty() {
        parts.push(trailing_trimmed.to_string());
    }
    parts.join("\n\n")
}

fn forbidden(detail: &str) -> Response {
    let mut response = (StatusCode::FORBIDDEN, detail.to_string()).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain"),
    );
    response
}

fn ok_twiml() -> Response {
    let mut response = TWIML_EMPTY_RESPONSE.into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn twilio_classifies_recipient_opted_out_as_forbidden() {
        let body = r#"{"code":21610,"message":"unsubscribed"}"#;
        let e = TwilioError::from_http(400, body).to_channel_error(body.into());
        assert_eq!(e.kind, ChannelErrorKind::Forbidden);
    }

    #[test]
    fn twilio_classifies_invalid_to_number_as_payload_invalid() {
        let body = r#"{"code":21211,"message":"invalid 'To'"}"#;
        let e = TwilioError::from_http(400, body).to_channel_error(body.into());
        assert_eq!(e.kind, ChannelErrorKind::PayloadInvalid);
    }

    #[test]
    fn twilio_classifies_unknown_destination_as_not_found() {
        let body = r#"{"code":30005,"message":"unknown destination"}"#;
        let e = TwilioError::from_http(400, body).to_channel_error(body.into());
        assert_eq!(e.kind, ChannelErrorKind::NotFound);
    }

    #[test]
    fn twilio_classifies_message_too_long_as_payload_too_large() {
        let body = r#"{"code":21611,"message":"too long"}"#;
        let e = TwilioError::from_http(400, body).to_channel_error(body.into());
        assert_eq!(e.kind, ChannelErrorKind::PayloadTooLarge);
    }

    #[test]
    fn twilio_classifies_rate_limit_as_transient() {
        let body = r#"{"code":20429,"message":"too many requests"}"#;
        let e = TwilioError::from_http(429, body).to_channel_error(body.into());
        assert_eq!(e.kind, ChannelErrorKind::Transient);
    }

    #[test]
    fn twilio_classifies_handset_unreachable_as_transient() {
        let body = r#"{"code":30003,"message":"unreachable"}"#;
        let e = TwilioError::from_http(400, body).to_channel_error(body.into());
        assert_eq!(e.kind, ChannelErrorKind::Transient);
    }

    #[test]
    fn twilio_falls_back_to_http_status_when_body_unparseable() {
        let e = TwilioError::from_http(500, "<html>upstream gateway</html>")
            .to_channel_error("transient".into());
        assert_eq!(e.kind, ChannelErrorKind::Transient);
        let e = TwilioError::from_http(401, "no body").to_channel_error("auth".into());
        assert_eq!(e.kind, ChannelErrorKind::Unauthorized);
    }

    #[test]
    fn twilio_unknown_code_defaults_to_transient() {
        let body = r#"{"code":999999,"message":"never seen"}"#;
        let e = TwilioError::from_http(400, body).to_channel_error(body.into());
        assert_eq!(e.kind, ChannelErrorKind::Transient);
    }

    #[test]
    fn manifest_has_required_fields_with_default_from() {
        let m = SmsAdapterFactory.manifest();
        assert_eq!(m.id, "sms");
        assert_eq!(m.display_name, "Twilio SMS");
        let by_name = |name: &str| {
            m.config_fields
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("field {name} missing"))
        };
        let sid = by_name("account_sid");
        assert!(sid.is_required && sid.is_secret);
        assert_eq!(
            sid.default_from,
            Some(ConfigRef {
                section: "voice".into(),
                field: "twilio_account_sid".into(),
            })
        );
        assert_eq!(sid.default_resolved, None);
        let token = by_name("auth_token");
        assert!(token.is_required && token.is_secret);
        assert_eq!(
            token.default_from,
            Some(ConfigRef {
                section: "voice".into(),
                field: "twilio_auth_token".into(),
            })
        );
        let from = by_name("from_number");
        assert!(from.is_required && !from.is_secret);
        assert_eq!(
            from.default_from,
            Some(ConfigRef {
                section: "voice".into(),
                field: "twilio_from_number".into(),
            })
        );
    }

    #[test]
    fn factory_create_with_valid_config_succeeds() {
        let cfg = json!({
            "account_sid": "ACxxx",
            "auth_token": "tok",
            "from_number": "+15550000000",
        });
        SmsAdapterFactory
            .create(cfg)
            .expect("valid config should produce a Channel");
    }

    #[test]
    fn factory_create_rejects_missing_account_sid() {
        let cfg = json!({"auth_token": "tok", "from_number": "+1"});
        assert!(matches!(
            SmsAdapterFactory.create(cfg),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn factory_create_rejects_missing_auth_token() {
        let cfg = json!({"account_sid": "AC", "from_number": "+1"});
        assert!(matches!(
            SmsAdapterFactory.create(cfg),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn factory_create_rejects_missing_from_number() {
        let cfg = json!({"account_sid": "AC", "auth_token": "tok"});
        assert!(matches!(
            SmsAdapterFactory.create(cfg),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn parse_external_id_e164() {
        assert_eq!(
            parse_external_id("sms:+15551234567").unwrap(),
            "+15551234567"
        );
    }

    #[test]
    fn parse_external_id_rejects_garbage() {
        assert!(parse_external_id("nonsense").is_err());
        assert!(parse_external_id("sms:").is_err());
        assert!(parse_external_id("dm:+15551234567").is_err());
    }

    #[test]
    fn webhook_params_extracts_known_fields() {
        let pairs = vec![
            ("From".into(), "+1".into()),
            ("Body".into(), "hi".into()),
            ("MessageSid".into(), "SM1".into()),
            ("NumMedia".into(), "2".into()),
            ("MessageStatus".into(), "delivered".into()),
            ("ErrorCode".into(), "30001".into()),
            ("ErrorMessage".into(), "queue overflow".into()),
            ("Unknown".into(), "ignored".into()),
        ];
        let p = TwilioWebhook::from_pairs(&pairs);
        assert_eq!(p.from, "+1");
        assert_eq!(p.body, "hi");
        assert_eq!(p.message_sid, "SM1");
        assert_eq!(p.num_media, "2");
        assert_eq!(p.message_status, "delivered");
        assert_eq!(p.error_code, "30001");
        assert_eq!(p.error_message, "queue overflow");
    }

    #[test]
    fn webhook_params_defaults_num_media_to_zero() {
        let p = TwilioWebhook::from_pairs(&[]);
        assert_eq!(p.num_media, "0");
        assert_eq!(p.from, "");
    }

    #[test]
    fn is_status_callback_true_for_twilio_status_payload() {
        let p = TwilioWebhook {
            message_status: "delivered".into(),
            message_sid: "SM1".into(),
            num_media: "0".into(),
            ..Default::default()
        };
        assert!(p.is_status_callback());
    }

    #[test]
    fn is_status_callback_false_for_inbound_with_received_status() {
        let p = TwilioWebhook {
            message_status: "received".into(),
            from: "+15551234567".into(),
            body: "hello".into(),
            message_sid: "SM2".into(),
            ..Default::default()
        };
        assert!(!p.is_status_callback());
    }

    #[test]
    fn is_status_callback_false_for_inbound_mms_with_no_body() {
        let p = TwilioWebhook {
            message_status: "received".into(),
            from: "+15551234567".into(),
            body: String::new(),
            num_media: "1".into(),
            message_sid: "SM3".into(),
            ..Default::default()
        };
        assert!(!p.is_status_callback());
    }

    #[test]
    fn is_status_callback_false_for_empty_ping() {
        let p = TwilioWebhook::default();
        assert!(!p.is_status_callback());
    }

    #[test]
    fn msg_id_from_query_round_trips_uuid() {
        let id = "5c6450c3-19aa-4ab2-84fd-b08b7359f81d";
        let q = format!("msg_id={id}");
        assert_eq!(msg_id_from_query(Some(&q)).as_deref(), Some(id));
        assert_eq!(msg_id_from_query(None), None);
        assert_eq!(msg_id_from_query(Some("other=1")), None);
    }

    #[test]
    fn status_callback_url_appends_msg_id_query() {
        let url = status_callback_url("https://x.com/api/webhooks/channels/sms/abc", "msg-1");
        assert_eq!(
            url,
            "https://x.com/api/webhooks/channels/sms/abc?msg_id=msg-1",
        );
    }

    #[test]
    fn format_carrier_error_with_message() {
        let p = TwilioWebhook {
            message_status: "failed".into(),
            error_code: "30007".into(),
            error_message: "carrier rejected".into(),
            ..Default::default()
        };
        assert_eq!(p.format_carrier_error(), "carrier rejected (failed 30007)");
    }

    #[test]
    fn format_carrier_error_without_message() {
        let p = TwilioWebhook {
            message_status: "undelivered".into(),
            error_code: "30005".into(),
            ..Default::default()
        };
        assert_eq!(p.format_carrier_error(), "Twilio undelivered (30005)");
    }

    fn fixture_signature() -> (String, String, Vec<(String, String)>, String) {
        let token = "12345".to_string();
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2".to_string();
        let params: Vec<(String, String)> = vec![
            ("CallSid".into(), "CA1234567890ABCDE".into()),
            ("Caller".into(), "+14158675309".into()),
            ("Digits".into(), "1234".into()),
            ("From".into(), "+14158675309".into()),
            ("To".into(), "+18005551212".into()),
        ];
        let mut canonical = String::from(&url);
        let mut sorted = params.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in &sorted {
            canonical.push_str(k);
            canonical.push_str(v);
        }
        let mut mac = Hmac::<Sha1>::new_from_slice(token.as_bytes()).unwrap();
        mac.update(canonical.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        (token, url, params, sig)
    }

    #[test]
    fn verify_twilio_signature_known_vector() {
        let (token, url, params, sig) = fixture_signature();
        assert!(verify_twilio_signature(&token, &url, &params, &sig));
    }

    #[test]
    fn verify_twilio_signature_mismatch_returns_false() {
        let (token, url, params, _sig) = fixture_signature();
        assert!(!verify_twilio_signature(
            &token,
            &url,
            &params,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA="
        ));
    }

    #[test]
    fn verify_twilio_signature_param_reorder_still_validates() {
        let (token, url, params, sig) = fixture_signature();
        let mut shuffled = params;
        shuffled.reverse();
        assert!(verify_twilio_signature(&token, &url, &shuffled, &sig));
    }

    #[test]
    fn canonical_webhook_url_includes_query_when_present() {
        let base = "https://x.com/api/webhooks/channels/ch:1";
        assert_eq!(canonical_webhook_url(base, None), base);
        assert_eq!(
            canonical_webhook_url(base, Some("a=1&b=2")),
            format!("{base}?a=1&b=2"),
        );
    }

    fn tool_call_with_turn_text(text: Option<&str>) -> crate::inference::tool_call::ToolCall {
        crate::inference::tool_call::ToolCall {
            id: "tc-1".into(),
            chat_id: "chat-1".into(),
            message_id: "msg-1".into(),
            turn: 0,
            provider_call_id: "pc-1".into(),
            name: "any".into(),
            arguments: serde_json::Value::Null,
            result: String::new(),
            success: true,
            duration_ms: 0,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: text.map(String::from),
            signature: None,
            turn_reasoning: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn compose_sms_body_joins_turn_texts_and_trailing_with_blank_line() {
        let tcs = vec![
            tool_call_with_turn_text(Some("hello")),
            tool_call_with_turn_text(Some("here is the result")),
        ];
        let body = compose_sms_body(&tcs, "anything else?");
        assert_eq!(body, "hello\n\nhere is the result\n\nanything else?");
    }

    #[test]
    fn compose_sms_body_skips_empty_and_whitespace_turn_texts() {
        let tcs = vec![
            tool_call_with_turn_text(Some("")),
            tool_call_with_turn_text(Some("   \n  ")),
            tool_call_with_turn_text(None),
            tool_call_with_turn_text(Some("real")),
        ];
        let body = compose_sms_body(&tcs, "tail");
        assert_eq!(body, "real\n\ntail");
    }

    #[test]
    fn compose_sms_body_returns_only_trailing_when_no_tools() {
        let body = compose_sms_body(&[], "just trailing");
        assert_eq!(body, "just trailing");
    }

    #[test]
    fn compose_sms_body_returns_only_turn_texts_when_trailing_empty() {
        let tcs = vec![
            tool_call_with_turn_text(Some("a")),
            tool_call_with_turn_text(Some("b")),
        ];
        let body = compose_sms_body(&tcs, "");
        assert_eq!(body, "a\n\nb");
    }

    #[test]
    fn compose_sms_body_empty_when_no_input() {
        let body = compose_sms_body(&[], "");
        assert_eq!(body, "");
    }

    #[test]
    fn compose_sms_body_strips_markdown_formatting() {
        let tcs = vec![tool_call_with_turn_text(Some("**bold** here"))];
        let body = compose_sms_body(&tcs, "");
        assert!(!body.contains("**"));
        assert!(body.contains("bold"));
    }
}
