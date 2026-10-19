pub use crate::inference::hitl::{
    Hitl, HitlDelivery, HitlOutcome, HitlRequest, HitlResponse, ResolveOutcome, VaultGrant,
};

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::Mutex;

use super::models::{ChannelAdapter, ChannelCtx};
use super::outbound::OutboundDeliveryService;
use super::supervisor::ChannelManager;

/// HITL resolution engine. Wraps `Harness::resolve_and_resume` + resume, then re-delivers
/// any remaining pending HITLs over the live adapter (resolving it through a `Weak` handle
/// to the supervisor's slot map — `Weak` so this shared engine never keeps the map, and
/// hence the supervisor, alive).
pub struct HitlDeliveryService {
    chat_service: crate::chat::service::ChatService,
    harness: Arc<crate::agent::harness::Harness>,
    task_executor: Arc<crate::agent::task::executor::TaskExecutor>,
    outbound: Arc<OutboundDeliveryService>,
    channels: Weak<Mutex<HashMap<String, Arc<ChannelManager>>>>,
}

impl HitlDeliveryService {
    pub(super) fn new(
        chat_service: crate::chat::service::ChatService,
        harness: Arc<crate::agent::harness::Harness>,
        task_executor: Arc<crate::agent::task::executor::TaskExecutor>,
        outbound: Arc<OutboundDeliveryService>,
        channels: Weak<Mutex<HashMap<String, Arc<ChannelManager>>>>,
    ) -> Self {
        Self {
            chat_service,
            harness,
            task_executor,
            outbound,
            channels,
        }
    }

    /// Full channel-side HITL resolution: harness resolve/resume, then re-deliver any
    /// remaining pending HITLs over the live adapter. Idempotent re-delivery.
    pub async fn resolve(
        &self,
        tool_call_id: &str,
        response: HitlResponse,
    ) -> Result<ResolveOutcome, crate::core::error::AppError> {
        let outcome = self.resolve_and_resume(tool_call_id, response).await?;
        if let Ok(Some(te)) = self.chat_service.get_tool_call(tool_call_id).await {
            let _ = self.outbound.ensure_pending_delivery(&te.message_id).await;
            if let Ok(Some(chat)) = self.chat_service.find_chat(&te.chat_id).await
                && let Some(channel_id) = chat.channel_id.as_deref()
                && let Some((adapter, ctx)) = self.running_adapter(channel_id).await
            {
                let _ = self
                    .outbound
                    .deliver_pending_hitls(&chat, &te.message_id, adapter.as_ref(), &ctx)
                    .await;
            }
        }
        Ok(outcome)
    }

    /// Resolve the live adapter+ctx for a channel via the supervisor's slot map.
    async fn running_adapter(
        &self,
        channel_id: &str,
    ) -> Option<(Arc<dyn ChannelAdapter>, ChannelCtx)> {
        let map = self.channels.upgrade()?;
        let manager = {
            let map = map.lock().await;
            map.get(channel_id).cloned()?
        };
        manager.live_handle().await
    }

    /// The harness core of HITL resolution: resolve + resume.
    pub async fn resolve_and_resume(
        &self,
        tool_call_id: &str,
        response: HitlResponse,
    ) -> Result<ResolveOutcome, crate::core::error::AppError> {
        let outcome = self
            .harness
            .resolve_and_resume(tool_call_id, response)
            .await?;
        if let ResolveOutcome::Resolved {
            should_resume: true,
            user_id,
            chat_id,
            message_id,
            task_id,
        } = &outcome
        {
            let h = self.harness.clone();
            let exec = self.task_executor.clone();
            let (u, c, m, tid) = (
                user_id.clone(),
                chat_id.clone(),
                message_id.clone(),
                task_id.clone(),
            );
            let cancel_token = if tid.is_none() {
                Some(h.active_sessions.register(&c).await)
            } else {
                None
            };
            tokio::spawn(async move {
                if let Some(tid) = tid {
                    let _ = exec.resume_task_by_id(&tid).await;
                } else if let Some(token) = cancel_token
                    && let Err(e) = h.resume_with_cancel(&u, &c, &m, token).await
                {
                    tracing::error!(error = %e, chat_id = %c, "Failed to resume chat after HITL resolve");
                }
            });
        }
        Ok(outcome)
    }
}

/// Closed render taxonomy. Channel adapters branch on this, never on
/// `HitlRequest` — new request variants only need a [`kind_for`] mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitlKind {
    Approval,
    Choice {
        options: Vec<String>,
    },
    /// No in-channel affordance — adapter posts the URL and the user resolves
    /// on the web (tools needing server-side pickers like vault selection).
    External,
}

pub fn kind_for(req: &HitlRequest) -> HitlKind {
    match req {
        HitlRequest::Question { options } => HitlKind::Choice {
            options: options.clone(),
        },
        HitlRequest::Takeover { .. } => HitlKind::Choice {
            options: vec!["Done".to_string()],
        },
        HitlRequest::App { .. } => HitlKind::Approval,
        HitlRequest::Credential { .. } => HitlKind::External,
    }
}

pub fn render_default_text(hitl: &Hitl) -> String {
    format!("{}\n\n{}", hitl.prompt, hitl.url)
}

/// Per-kind body for text-only adapters (Signal, WhatsApp user, SMS) which
/// can't render native buttons. Approval is currently App-deploy only; the
/// YES/NO hint cues users to reply with a parseable word (`parse_yes_no`)
/// since they have no button to tap.
pub fn render_text(hitl: &Hitl) -> String {
    match kind_for(&hitl.request) {
        HitlKind::Approval => {
            format!("{}\n\nReply YES or NO", hitl.prompt)
        }
        HitlKind::Choice { .. } => hitl.prompt.clone(),
        HitlKind::External => render_default_text(hitl),
    }
}

/// Reply-based HITL resolution for text-only channel adapters (WhatsApp,
/// Signal, SMS reply).
///
/// - `quoted_external_message_id`: when the inbound message is a quote-reply,
///   the provider's identifier for the quoted message (Slack `ts`, WhatsApp
///   `stanza_id`, etc.). The helper matches it against `hitl.delivery
///   .external_message_id` to pick the specific HITL the user is answering.
/// - Without a quote, the helper resolves only if exactly one HITL is pending
///   AND delivered (sequential cadence keeps this to one in practice).
///
/// Per-kind dispatch:
/// - `Choice` — text becomes `HitlResponse::Choice(text)`.
/// - `Approval` — text parsed via `parse_yes_no`. Ambiguous text returns
///   `Ok(None)` so the caller falls through to a normal user turn.
/// - `External` — returns `Ok(None)` (URL-only resolve path).
pub async fn try_resolve_inbound(
    chat_service: &crate::chat::service::ChatService,
    hitl_service: &HitlDeliveryService,
    chat_id: &str,
    quoted_external_message_id: Option<&str>,
    text: &str,
) -> Result<Option<ResolveOutcome>, crate::core::error::AppError> {
    let Some(msg) = chat_service.find_paused_message_for_chat(chat_id).await? else {
        return Ok(None);
    };
    let tool_calls = chat_service.get_tool_calls_by_message(&msg.id).await?;

    let candidates: Vec<_> = tool_calls
        .iter()
        .filter(|tc| {
            tc.hitl.as_ref().is_some_and(|h| {
                h.status == crate::inference::tool_call::ToolStatus::Pending && h.delivery.is_some()
            })
        })
        .collect();

    let target = match quoted_external_message_id {
        Some(qid) => candidates.into_iter().find(|tc| {
            tc.hitl
                .as_ref()
                .and_then(|h| h.delivery.as_ref())
                .is_some_and(|d| d.external_message_id == qid)
        }),
        None => {
            if candidates.len() == 1 {
                Some(candidates[0])
            } else {
                None
            }
        }
    };

    let Some(tc) = target else { return Ok(None) };
    let hitl = tc.hitl.as_ref().expect("filtered to Some hitl");
    let response = match kind_for(&hitl.request) {
        HitlKind::Choice { .. } => HitlResponse::Choice(text.to_string()),
        HitlKind::Approval => match parse_yes_no(text) {
            Some(b) => HitlResponse::Approval(b),
            None => return Ok(None),
        },
        HitlKind::External => return Ok(None),
    };

    let outcome = hitl_service.resolve(&tc.id, response).await?;
    Ok(Some(outcome))
}

/// Parse a free-text reply as yes/no. Exact match (lowercase, trimmed)
/// against the wordlist — substring would over-match phrases like "no
/// problem" containing "no". Anything not on the list returns `None` so the
/// caller can treat the reply as a regular user turn.
pub fn parse_yes_no(text: &str) -> Option<bool> {
    let t = text.trim().to_lowercase();
    const YES: &[&str] = &[
        "yes", "y", "yeah", "yep", "ok", "okay", "sure", "approve", "approved", "👍", "✅", "✔",
    ];
    const NO: &[&str] = &[
        "no", "n", "nope", "nah", "cancel", "decline", "declined", "reject", "rejected", "👎",
        "❌", "✖",
    ];
    if YES.iter().any(|w| *w == t) {
        Some(true)
    } else if NO.iter().any(|w| *w == t) {
        Some(false)
    } else {
        None
    }
}

/// Shared callback-data parser used by every channel adapter that renders
/// HITLs as buttons. Format:
///
///   r:{tool_call_id}:y       → HitlResponse::Approval(true)
///   r:{tool_call_id}:n       → HitlResponse::Approval(false)
///   r:{tool_call_id}:c:{idx} → HitlResponse::Choice(options[idx]) — requires
///                              a lookup against the ToolCall's HitlRequest to
///                              find the option string at that index.
pub async fn parse_resolve_callback_data(
    data: &str,
    chat_service: &crate::chat::service::ChatService,
) -> Result<(String, HitlResponse), crate::core::error::AppError> {
    let parts: Vec<&str> = data.splitn(4, ':').collect();
    match parts.as_slice() {
        ["r", tcid, "y"] => Ok((tcid.to_string(), HitlResponse::Approval(true))),
        ["r", tcid, "n"] => Ok((tcid.to_string(), HitlResponse::Approval(false))),
        ["r", tcid, "c", idx_str] => {
            let idx: usize = idx_str.parse().map_err(|_| {
                crate::core::error::AppError::Validation(format!("bad choice index: {idx_str}"))
            })?;
            let te = chat_service.get_tool_call(tcid).await?.ok_or_else(|| {
                crate::core::error::AppError::NotFound(format!("tool_call {tcid}"))
            })?;
            let hitl = te.hitl.as_ref().ok_or_else(|| {
                crate::core::error::AppError::Validation(format!("tool_call {tcid} has no HITL"))
            })?;
            let chosen = match &hitl.request {
                HitlRequest::Question { options } => {
                    options.get(idx).cloned().ok_or_else(|| {
                        crate::core::error::AppError::Validation(format!(
                            "option index {idx} out of range"
                        ))
                    })?
                }
                HitlRequest::Takeover { .. } => "Done".to_string(),
                _ => {
                    return Err(crate::core::error::AppError::Validation(
                        "Choice callback on non-Choice HITL".into(),
                    ));
                }
            };
            Ok((tcid.to_string(), HitlResponse::Choice(chosen)))
        }
        _ => Err(crate::core::error::AppError::Validation(format!(
            "malformed callback_data: {data}"
        ))),
    }
}

/// User-facing label for a `HitlResponse`. Shared across adapters so the
/// post-resolve message edit reflects what was picked, not a generic
/// "Resolved" placeholder.
pub fn response_display(response: &HitlResponse) -> String {
    match response {
        HitlResponse::Approval(true) => "✅ Yes".to_string(),
        HitlResponse::Approval(false) => "❌ No".to_string(),
        HitlResponse::Choice(text) => text.clone(),
        HitlResponse::Vault(VaultGrant::Granted { .. }) => "🔑 Granted".to_string(),
        HitlResponse::Vault(VaultGrant::Denied) => "🚫 Denied".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_for_question_returns_choice_with_options() {
        let req = HitlRequest::Question {
            options: vec!["a".into(), "b".into()],
        };
        match kind_for(&req) {
            HitlKind::Choice { options } => {
                assert_eq!(options, vec!["a".to_string(), "b".to_string()]);
            }
            _ => panic!("expected Choice"),
        }
    }

    #[test]
    fn kind_for_takeover_returns_choice_with_done() {
        let req = HitlRequest::Takeover {
            reason: "manual debug".into(),
            debugger_url: "https://x".into(),
        };
        match kind_for(&req) {
            HitlKind::Choice { options } => {
                assert_eq!(options, vec!["Done".to_string()]);
            }
            _ => panic!("expected Choice"),
        }
    }

    #[test]
    fn kind_for_service_approval_returns_approval() {
        let req = HitlRequest::App {
            action: "deploy".into(),
            manifest: serde_json::json!({}),
            previous_manifest: None,
        };
        assert_eq!(kind_for(&req), HitlKind::Approval);
    }

    #[test]
    fn kind_for_vault_pick_returns_external() {
        let req = HitlRequest::Credential {
            query: "postgres".into(),
            reason: "ETL".into(),
        };
        assert_eq!(kind_for(&req), HitlKind::External);
    }

    #[test]
    fn kind_for_question_with_empty_options_still_returns_choice() {
        let req = HitlRequest::Question { options: vec![] };
        match kind_for(&req) {
            HitlKind::Choice { options } => assert!(options.is_empty()),
            _ => panic!("expected Choice"),
        }
    }

    #[test]
    fn render_default_text_contains_prompt_and_url() {
        let h = Hitl {
            prompt: "Deploy notes?".into(),
            url: "https://app.example/chats/abc".into(),
            request: HitlRequest::App {
                action: "deploy".into(),
                manifest: serde_json::json!({}),
                previous_manifest: None,
            },
            status: crate::inference::tool_call::ToolStatus::Pending,
            response: None,
            delivery: None,
        };
        let text = render_default_text(&h);
        assert!(text.contains("Deploy notes?"));
        assert!(text.contains("https://app.example/chats/abc"));
    }

    #[test]
    fn response_display_approval_yes() {
        assert_eq!(response_display(&HitlResponse::Approval(true)), "✅ Yes");
    }

    #[test]
    fn response_display_approval_no() {
        assert_eq!(response_display(&HitlResponse::Approval(false)), "❌ No");
    }

    #[test]
    fn response_display_choice_returns_chosen_text() {
        let r = HitlResponse::Choice("eu".into());
        assert_eq!(response_display(&r), "eu");
    }

    #[test]
    fn parse_yes_no_recognises_yes_variants() {
        for s in ["yes", "Y", " yeah ", "OK", "👍", "approved"] {
            assert_eq!(parse_yes_no(s), Some(true), "expected Some(true) for {s:?}");
        }
    }

    #[test]
    fn parse_yes_no_recognises_no_variants() {
        for s in ["no", "N", " nope ", "Cancel", "👎", "rejected"] {
            assert_eq!(
                parse_yes_no(s),
                Some(false),
                "expected Some(false) for {s:?}"
            );
        }
    }

    #[test]
    fn parse_yes_no_returns_none_for_ambiguous_text() {
        for s in ["maybe", "yes please", "no problem", "i guess", ""] {
            assert_eq!(parse_yes_no(s), None, "expected None for {s:?}");
        }
    }
}
