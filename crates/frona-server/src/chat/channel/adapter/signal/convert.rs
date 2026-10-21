use std::sync::Arc;

use presage::Manager;
use presage::libsignal_service::content::{Content, ContentBody, Metadata};
use presage::libsignal_service::proto::DataMessage;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::Registered;
use presage::store::Store;
use tokio::sync::mpsc;

use crate::chat::channel::HitlDeliveryService;
use crate::chat::channel::models::ExternalMessage;
use crate::chat::service::ChatService;

use super::command::SignalCommand;
use super::external_id;

pub async fn handle<S: Store>(
    mgr: &mut Manager<S, Registered>,
    emit: &mpsc::Sender<ExternalMessage>,
    cmd_tx: &mpsc::Sender<SignalCommand>,
    content: Content,
    channel_id: &str,
    chat_service: &ChatService,
    hitl: &Arc<HitlDeliveryService>,
) {
    tracing::debug!(
        channel_id = %channel_id,
        sender = %content.metadata.sender.raw_uuid(),
        timestamp = %content.metadata.client_timestamp,
        body_kind = content_body_kind(&content.body),
        needs_receipt = content.metadata.needs_receipt,
        unidentified = content.metadata.unidentified_sender,
        "Signal inbound content received",
    );

    let ContentBody::DataMessage(dm) = content.body else {
        tracing::debug!(
            channel_id = %channel_id,
            "Signal inbound ignored (non-DataMessage variant)",
        );
        return;
    };
    let self_aci: ServiceId = Aci::from(mgr.registration_data().service_ids.aci).into();
    let Some(event) = shape_event(self_aci, &content.metadata, &dm) else {
        tracing::debug!(
            channel_id = %channel_id,
            sender_is_self = %(content.metadata.sender == self_aci),
            has_body = %dm.body.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false),
            attachments = dm.attachments.len(),
            "Signal inbound DataMessage skipped (self-echo, or empty body + no attachments)",
        );
        return;
    };

    let quoted_id: Option<String> = dm
        .quote
        .as_ref()
        .and_then(|q| q.id)
        .map(|ts| ts.to_string());
    let sender = content.metadata.sender;
    // Signal read receipts reference the sender's original message timestamp,
    // not the time the server received it.
    let inbound_ts = content.metadata.client_timestamp.timestamp_millis() as u64;

    // Spawn the resolve + emit work off the worker loop. The Signal worker
    // runs on a single-threaded runtime; if resolve_hitl reaches back into
    // on_pending_hitl → dispatch_text → cmd_tx.send + rx.await, the reply
    // can only be fulfilled by `command::handle` running on this same loop —
    // which would be blocked here. Spawn so the loop stays free to drain
    // outbound commands.
    let emit = emit.clone();
    let cmd_tx = cmd_tx.clone();
    let chat_service = chat_service.clone();
    let hitl = hitl.clone();
    let channel_id = channel_id.to_string();
    tokio::spawn(async move {
        let send_receipt = || async {
            let _ = cmd_tx
                .send(SignalCommand::SendReadReceipt {
                    sender,
                    timestamps: vec![inbound_ts],
                })
                .await;
        };

        if let Ok(Some(chat)) = chat_service
            .find_chat_by_channel_external_id(&channel_id, &event.external_chat_id)
            .await
        {
            match crate::chat::channel::hitl::try_resolve_inbound(
                &chat_service,
                &hitl,
                &chat.id,
                quoted_id.as_deref(),
                &event.content,
            )
            .await
            {
                Ok(Some(crate::inference::hitl::ResolveOutcome::Resolved { .. })) => {
                    tracing::info!(
                        channel_id = %channel_id,
                        signal_chat = %event.external_chat_id,
                        quoted_id = ?quoted_id,
                        "Signal inbound consumed as HITL resolution",
                    );
                    send_receipt().await;
                    return;
                }
                Ok(Some(crate::inference::hitl::ResolveOutcome::AlreadyResolved)) => {
                    tracing::info!(
                        channel_id = %channel_id,
                        signal_chat = %event.external_chat_id,
                        "Signal inbound matched an already-resolved HITL; skipping",
                    );
                    send_receipt().await;
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "Signal try_resolve_inbound failed; falling through to emit",
                    );
                }
            }
        }

        tracing::info!(
            channel_id = %channel_id,
            from = %event.sender_address,
            signal_chat = %event.external_chat_id,
            "Signal inbound accepted - emitting to inbound pipeline",
        );
        if let Err(e) = emit.send(event).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Signal inbound emit failed (pipeline closed)",
            );
            return;
        }
        send_receipt().await;
    });
}

fn content_body_kind(body: &ContentBody) -> &'static str {
    match body {
        ContentBody::DataMessage(_) => "DataMessage",
        ContentBody::SynchronizeMessage(_) => "SynchronizeMessage",
        ContentBody::CallMessage(_) => "CallMessage",
        ContentBody::ReceiptMessage(_) => "ReceiptMessage",
        ContentBody::TypingMessage(_) => "TypingMessage",
        ContentBody::StoryMessage(_) => "StoryMessage",
        ContentBody::EditMessage(_) => "EditMessage",
        ContentBody::NullMessage(_) => "NullMessage",
        ContentBody::DecryptionErrorMessage(_) => "DecryptionErrorMessage",
    }
}

pub(super) fn shape_event(
    self_aci: ServiceId,
    meta: &Metadata,
    dm: &DataMessage,
) -> Option<ExternalMessage> {
    if meta.sender == self_aci {
        return None;
    }

    let body_text = dm.body.clone().unwrap_or_default();
    let has_text = !body_text.trim().is_empty();
    let attachment_markers: Vec<String> = dm
        .attachments
        .iter()
        .map(|a| {
            let kind = a.content_type.as_deref().unwrap_or("file");
            match a.file_name.as_deref() {
                Some(name) => format!("[attachment: {kind} {name}]"),
                None => format!("[attachment: {kind}]"),
            }
        })
        .collect();

    if !has_text && attachment_markers.is_empty() {
        return None;
    }

    let aci_uuid = meta.sender.raw_uuid();
    let external_chat_id = match &dm.group_v2 {
        Some(g) => external_id::group(g.master_key.as_deref().unwrap_or_default()),
        None => external_id::dm(aci_uuid),
    };

    let mut content = body_text;
    if !attachment_markers.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&attachment_markers.join("\n"));
    }

    let sender_id = aci_uuid.to_string();
    Some(ExternalMessage {
        external_chat_id,
        sender_address: sender_id.clone(),
        sender_external_id: Some(sender_id),
        sender_display_name: None,
        content,
        attachments: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::proto::{AttachmentPointer, GroupContextV2};
    use uuid::Uuid;

    fn aci(s: &str) -> ServiceId {
        Aci::from(Uuid::parse_str(s).unwrap()).into()
    }

    fn meta(sender: ServiceId, destination: ServiceId, ts: u64) -> Metadata {
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts as i64).unwrap();
        Metadata {
            sender,
            sender_device: presage::libsignal_service::protocol::DeviceId::new(1).unwrap(),
            destination,
            server_guid: None,
            client_timestamp: ts,
            server_timestamp: ts,
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            pni_verified: None,
        }
    }

    fn dm_text(body: &str) -> DataMessage {
        DataMessage {
            body: Some(body.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn shape_skips_self_echo() {
        let self_aci = aci("3b9b1cfc-2dbe-4d2c-9c8a-0a8f6fffeeaa");
        let m = meta(self_aci, self_aci, 1);
        assert!(shape_event(self_aci, &m, &dm_text("echo")).is_none());
    }

    #[test]
    fn shape_skips_empty_body_no_attachments() {
        let me = aci("3b9b1cfc-2dbe-4d2c-9c8a-0a8f6fffeeaa");
        let them = aci("cccccccc-2dbe-4d2c-9c8a-0a8f6ffffeee");
        let m = meta(them, me, 1);
        assert!(shape_event(me, &m, &dm_text("   ")).is_none());
    }

    #[test]
    fn shape_dm_text_emits_dm_external_id() {
        let me = aci("3b9b1cfc-2dbe-4d2c-9c8a-0a8f6fffeeaa");
        let them_uuid = "cccccccc-2dbe-4d2c-9c8a-0a8f6ffffeee";
        let them = aci(them_uuid);
        let m = meta(them, me, 1);
        let event = shape_event(me, &m, &dm_text("hi")).unwrap();
        assert_eq!(event.external_chat_id, format!("dm:{them_uuid}"));
        assert_eq!(event.content, "hi");
        assert_eq!(event.sender_address, them_uuid);
        assert_eq!(event.sender_external_id.as_deref(), Some(them_uuid));
    }

    #[test]
    fn shape_group_message_emits_group_external_id() {
        let me = aci("3b9b1cfc-2dbe-4d2c-9c8a-0a8f6fffeeaa");
        let them = aci("cccccccc-2dbe-4d2c-9c8a-0a8f6ffffeee");
        let m = meta(them, me, 1);
        let master_key = vec![9u8; 32];
        let dm = DataMessage {
            body: Some("group hi".into()),
            group_v2: Some(GroupContextV2 {
                master_key: Some(master_key.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let event = shape_event(me, &m, &dm).unwrap();
        assert!(
            event.external_chat_id.starts_with("group:"),
            "got {}",
            event.external_chat_id
        );
        assert_eq!(event.content, "group hi");
    }

    #[test]
    fn shape_attachments_become_markers() {
        let me = aci("3b9b1cfc-2dbe-4d2c-9c8a-0a8f6fffeeaa");
        let them = aci("cccccccc-2dbe-4d2c-9c8a-0a8f6ffffeee");
        let m = meta(them, me, 1);
        let dm = DataMessage {
            body: Some("caption".into()),
            attachments: vec![AttachmentPointer {
                content_type: Some("image/jpeg".into()),
                file_name: Some("cat.jpg".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let event = shape_event(me, &m, &dm).unwrap();
        assert!(event.content.contains("caption"));
        assert!(event.content.contains("[attachment: image/jpeg cat.jpg]"));
        // bytes are intentionally NOT downloaded in v1
        assert!(event.attachments.is_empty());
    }

    #[test]
    fn shape_attachment_only_message_still_emitted() {
        let me = aci("3b9b1cfc-2dbe-4d2c-9c8a-0a8f6fffeeaa");
        let them = aci("cccccccc-2dbe-4d2c-9c8a-0a8f6ffffeee");
        let m = meta(them, me, 1);
        let dm = DataMessage {
            body: None,
            attachments: vec![AttachmentPointer {
                content_type: Some("application/pdf".into()),
                file_name: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let event = shape_event(me, &m, &dm).unwrap();
        assert_eq!(event.content, "[attachment: application/pdf]");
    }

    #[test]
    fn quoted_id_extracted_from_quote() {
        use presage::libsignal_service::proto::data_message::Quote;
        let dm = DataMessage {
            body: Some("yes".into()),
            quote: Some(Quote {
                id: Some(1_700_000_000_123),
                ..Default::default()
            }),
            ..Default::default()
        };
        let quoted = dm
            .quote
            .as_ref()
            .and_then(|q| q.id)
            .map(|ts| ts.to_string());
        assert_eq!(quoted.as_deref(), Some("1700000000123"));
    }

    #[test]
    fn quoted_id_is_none_when_no_quote() {
        let dm = dm_text("plain reply");
        let quoted = dm
            .quote
            .as_ref()
            .and_then(|q| q.id)
            .map(|ts| ts.to_string());
        assert_eq!(quoted, None);
    }
}
