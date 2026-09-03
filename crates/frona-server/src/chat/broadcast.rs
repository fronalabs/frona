use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::response::sse::Event;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};

use crate::inference::tool_loop::InferenceEventKind;
use crate::notification::models::Notification;

use super::message::models::MessageResponse;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntityAction {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum BroadcastEventKind {
    Inference(InferenceEventKind),
    Title {
        title: String,
    },
    NewNotification {
        notification: Notification,
    },
    ChatMessage {
        message: MessageResponse,
    },
    TaskUpdate {
        task_id: String,
        status: String,
        title: String,
        chat_id: Option<String>,
        source_chat_id: Option<String>,
        result_summary: Option<String>,
    },
    InferenceCount {
        count: usize,
    },
    ActivityChanged,
    EntityUpdated {
        table: String,
        record_id: String,
        action: EntityAction,
        space_id: Option<String>,
        fields: Option<serde_json::Value>,
    },
    /// Per-inference-call usage event fired from `UsageService::record`.
    /// Only dispatched when the row has a `chat_id` so we have somewhere to send it
    /// - rootless rows (`Compaction::User`, `Compaction::Space`) still hit the DB
    ///   and Prometheus, they just don't broadcast.
    UsageRecorded(UsageRecorded),
}

/// Payload for `BroadcastEventKind::UsageRecorded`. Carries the slim subset of
/// the `InferenceUsage` row that frontends need to update the per-message
/// footer and chat-header total pill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecorded {
    pub chat_id: String,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub message_id: Option<String>,
    pub kind_tag: String,
    pub model_ref: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub output_tokens_per_second: Option<f64>,
    pub retry_overhead_ms: u64,
    pub retry_count: u32,
    pub fallback_index: u8,
}

#[derive(Debug, Clone)]
pub struct BroadcastEvent {
    pub user_id: String,
    pub chat_id: Option<String>,
    pub space_id: Option<String>,
    pub kind: BroadcastEventKind,
}

type SseSender = mpsc::UnboundedSender<Result<Event, Infallible>>;
type SessionRegistry = Arc<RwLock<HashMap<String, Vec<SseSender>>>>;
/// TTL cache that buffers SSE events after all of a user's senders disconnect.
/// When the user reconnects, buffered events are drained into the new sender.
type PendingEventsCache = Arc<moka::sync::Cache<String, Arc<Mutex<Vec<Event>>>>>;

/// Pre-serialized event ready for the dispatcher to route.
struct DispatchEvent {
    user_id: String,
    is_global: bool,
    sse: Event,
}

#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::UnboundedSender<DispatchEvent>,
    bus: crate::core::event_bus::EventBus<BroadcastEvent>,
    user_id: String,
    chat_id: String,
    space_id: Option<String>,
}

impl EventSender {
    /// A sender wired to nothing - for detached (chatless) inference that streams
    /// no events. The mpsc receiver is dropped and the bus has no subscribers, so
    /// `send`/`send_kind` are silent no-ops (both already ignore the send result).
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            tx,
            bus: crate::core::event_bus::EventBus::new(),
            user_id: String::new(),
            chat_id: String::new(),
            space_id: None,
        }
    }

    pub fn send(&self, event: crate::inference::tool_loop::InferenceEvent) {
        let broadcast = BroadcastEvent {
            user_id: self.user_id.clone(),
            chat_id: Some(self.chat_id.clone()),
            space_id: self.space_id.clone(),
            kind: BroadcastEventKind::Inference(event.kind),
        };
        self.bus.publish(broadcast.clone());
        if let Some(sse) = map_event_to_sse(&broadcast) {
            let _ = self.tx.send(DispatchEvent {
                user_id: broadcast.user_id,
                is_global: false,
                sse,
            });
        }
    }

    pub fn send_kind(&self, kind: BroadcastEventKind) {
        let broadcast = BroadcastEvent {
            user_id: self.user_id.clone(),
            chat_id: Some(self.chat_id.clone()),
            space_id: self.space_id.clone(),
            kind,
        };
        self.bus.publish(broadcast.clone());
        if let Some(sse) = map_event_to_sse(&broadcast) {
            let _ = self.tx.send(DispatchEvent {
                user_id: broadcast.user_id,
                is_global: false,
                sse,
            });
        }
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

#[derive(Clone)]
pub struct BroadcastService {
    tx: mpsc::UnboundedSender<DispatchEvent>,
    sessions: SessionRegistry,
    pending_events: PendingEventsCache,
    bus: crate::core::event_bus::EventBus<BroadcastEvent>,
}

impl Default for BroadcastService {
    fn default() -> Self {
        Self::new()
    }
}

fn sse_event(name: &str, data: impl serde::Serialize) -> Event {
    Event::default().event(name).json_data(data).unwrap()
}

pub(crate) fn map_event_to_sse(event: &BroadcastEvent) -> Option<Event> {
    match &event.kind {
        BroadcastEventKind::Inference(kind) => {
            let chat_id = event.chat_id.as_deref().unwrap_or("");
            match kind {
                InferenceEventKind::Text(text) => Some(sse_event(
                    "token",
                    serde_json::json!({ "chat_id": chat_id, "content": text }),
                )),
                InferenceEventKind::ToolCall {
                    id,
                    provider_call_id,
                    name,
                    arguments,
                    description,
                } => Some(sse_event(
                    "tool_call",
                    serde_json::json!({
                        "chat_id": chat_id,
                        "id": id,
                        "provider_call_id": provider_call_id,
                        "name": name,
                        "arguments": arguments,
                        "description": description,
                    }),
                )),
                InferenceEventKind::Reasoning(text) => Some(sse_event(
                    "reasoning",
                    serde_json::json!({ "chat_id": chat_id, "content": text }),
                )),
                InferenceEventKind::ToolResult {
                    name,
                    result,
                    success,
                } => {
                    let summary: String = result.chars().take(200).collect();
                    Some(sse_event(
                        "tool_result",
                        serde_json::json!({ "chat_id": chat_id, "name": name, "success": success, "summary": summary }),
                    ))
                }
                InferenceEventKind::EntityUpdated {
                    table,
                    record_id,
                    fields,
                } => Some(sse_event(
                    "entity_updated",
                    serde_json::json!({
                        "chat_id": chat_id,
                        "table": table,
                        "record_id": record_id,
                        "fields": fields,
                    }),
                )),
                InferenceEventKind::Retry {
                    retry_after_ms,
                    reason,
                } => Some(sse_event(
                    "retry",
                    serde_json::json!({
                        "chat_id": chat_id,
                        "retry_after_secs": retry_after_ms / 1000,
                        "reason": reason,
                    }),
                )),
                InferenceEventKind::Start => Some(sse_event(
                    "inference_start",
                    serde_json::json!({ "chat_id": chat_id }),
                )),
                InferenceEventKind::Done { message } => Some(sse_event(
                    "inference_done",
                    serde_json::json!({ "chat_id": chat_id, "message": message }),
                )),
                InferenceEventKind::Cancelled { reason } => Some(sse_event(
                    "inference_cancelled",
                    serde_json::json!({ "chat_id": chat_id, "reason": reason }),
                )),
                InferenceEventKind::Failed { error } => Some(sse_event(
                    "inference_error",
                    serde_json::json!({ "chat_id": chat_id, "error": error }),
                )),
                InferenceEventKind::Paused { reason, message } => Some(sse_event(
                    "inference_paused",
                    serde_json::json!({
                        "chat_id": chat_id,
                        "reason": reason,
                        "message": message,
                    }),
                )),
                InferenceEventKind::Resume { message } => Some(sse_event(
                    "inference_resume",
                    serde_json::json!({ "chat_id": chat_id, "message": message }),
                )),
            }
        }
        BroadcastEventKind::Title { title } => {
            let chat_id = event.chat_id.as_deref().unwrap_or("");
            Some(sse_event(
                "title",
                serde_json::json!({ "chat_id": chat_id, "title": title }),
            ))
        }
        BroadcastEventKind::ChatMessage { message } => {
            let chat_id = event.chat_id.as_deref().unwrap_or("");
            Some(sse_event(
                "chat_message",
                serde_json::json!({ "chat_id": chat_id, "message": message }),
            ))
        }
        BroadcastEventKind::TaskUpdate {
            task_id,
            status,
            title,
            chat_id,
            source_chat_id,
            result_summary,
        } => Some(sse_event(
            "task_update",
            serde_json::json!({
                "task_id": task_id,
                "status": status,
                "title": title,
                "chat_id": chat_id,
                "source_chat_id": source_chat_id,
                "result_summary": result_summary,
            }),
        )),
        BroadcastEventKind::InferenceCount { count } => Some(sse_event(
            "inference_count",
            serde_json::json!({ "count": count }),
        )),
        BroadcastEventKind::ActivityChanged => {
            Some(sse_event("activity_changed", serde_json::json!({})))
        }
        BroadcastEventKind::NewNotification { notification } => Some(sse_event(
            "notification",
            serde_json::json!({ "notification": notification }),
        )),
        BroadcastEventKind::EntityUpdated {
            table,
            record_id,
            action,
            space_id,
            fields,
        } => Some(sse_event(
            "entity_updated",
            serde_json::json!({
                "table": table,
                "record_id": record_id,
                "action": action,
                "space_id": space_id,
                "fields": fields,
            }),
        )),
        BroadcastEventKind::UsageRecorded(u) => Some(sse_event("usage_recorded", u)),
    }
}

impl BroadcastService {
    pub fn new() -> Self {
        Self::with_pending_events_secs(60)
    }

    pub fn with_pending_events_secs(secs: u64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let sessions: SessionRegistry = Arc::new(RwLock::new(HashMap::new()));
        let pending_events: PendingEventsCache = Arc::new(
            moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(secs.max(1)))
                .build(),
        );

        let sessions_clone = sessions.clone();
        let pending_events_clone = pending_events.clone();
        tokio::spawn(async move {
            Self::run_dispatcher(rx, sessions_clone, pending_events_clone).await;
        });

        let bus = crate::core::event_bus::EventBus::<BroadcastEvent>::new();
        Self {
            tx,
            sessions,
            pending_events,
            bus,
        }
    }

    pub fn subscribe_raw(&self) -> mpsc::UnboundedReceiver<BroadcastEvent> {
        self.bus.subscribe()
    }

    async fn run_dispatcher(
        mut rx: mpsc::UnboundedReceiver<DispatchEvent>,
        sessions: SessionRegistry,
        pending_events: PendingEventsCache,
    ) {
        while let Some(event) = rx.recv().await {
            let registry = sessions.read().await;
            if event.is_global {
                for senders in registry.values() {
                    for sender in senders {
                        let _ = sender.send(Ok(event.sse.clone()));
                    }
                }
            } else if let Some(senders) = registry.get(&event.user_id) {
                let mut dead_indices = Vec::new();
                for (i, sender) in senders.iter().enumerate() {
                    if sender.send(Ok(event.sse.clone())).is_err() {
                        dead_indices.push(i);
                    }
                }
                if !dead_indices.is_empty() {
                    drop(registry);
                    let mut registry = sessions.write().await;
                    if let Some(senders) = registry.get_mut(&event.user_id) {
                        for i in dead_indices.into_iter().rev() {
                            if i < senders.len() {
                                senders.swap_remove(i);
                            }
                        }
                        if senders.is_empty() {
                            registry.remove(&event.user_id);
                        }
                    }
                }
                let has_live = {
                    let reg = sessions.read().await;
                    reg.get(&event.user_id).is_some_and(|s| !s.is_empty())
                };
                if !has_live {
                    let buf = pending_events
                        .get_with(event.user_id.clone(), || Arc::new(Mutex::new(Vec::new())));
                    buf.lock().unwrap().push(event.sse);
                }
            } else {
                if let Some(buf) = pending_events.get(&event.user_id) {
                    buf.lock().unwrap().push(event.sse);
                }
            }
        }
    }

    fn dispatch(&self, event: BroadcastEvent) {
        self.bus.publish(event.clone());

        let is_global = matches!(event.kind, BroadcastEventKind::InferenceCount { .. });
        if let Some(sse) = map_event_to_sse(&event) {
            let _ = self.tx.send(DispatchEvent {
                user_id: event.user_id,
                is_global,
                sse,
            });
        }
    }

    pub fn create_event_sender(
        &self,
        user_id: &str,
        chat_id: &str,
        space_id: Option<String>,
    ) -> EventSender {
        EventSender {
            tx: self.tx.clone(),
            bus: self.bus.clone(),
            user_id: user_id.to_string(),
            chat_id: chat_id.to_string(),
            space_id,
        }
    }

    pub async fn register_session(&self, user_id: &str, sender: SseSender) {
        if let Some(buf) = self.pending_events.remove(user_id) {
            for event in buf.lock().unwrap().drain(..) {
                let _ = sender.send(Ok(event));
            }
        }
        let mut registry = self.sessions.write().await;
        registry
            .entry(user_id.to_string())
            .or_default()
            .push(sender);
    }

    pub fn send(&self, event: BroadcastEvent) {
        self.dispatch(event);
    }

    pub fn broadcast_chat_message(
        &self,
        user_id: &str,
        chat_id: &str,
        space_id: Option<String>,
        message: MessageResponse,
    ) {
        self.dispatch(BroadcastEvent {
            user_id: user_id.to_string(),
            chat_id: Some(chat_id.to_string()),
            space_id,
            kind: BroadcastEventKind::ChatMessage { message },
        });
    }

    /// Dispatched by `UsageService::record` after persisting a row.
    /// Always carries a `chat_id` - callers gate on `row.chat_id.is_some()`.
    pub fn broadcast_usage_recorded(&self, usage: UsageRecorded) {
        let user_id = usage.user_id.clone();
        let chat_id = usage.chat_id.clone();
        self.dispatch(BroadcastEvent {
            user_id,
            chat_id: Some(chat_id),
            space_id: None,
            kind: BroadcastEventKind::UsageRecorded(usage),
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn broadcast_task_update(
        &self,
        user_id: &str,
        task_id: &str,
        status: &str,
        title: &str,
        chat_id: Option<&str>,
        source_chat_id: Option<&str>,
        result_summary: Option<&str>,
    ) {
        self.dispatch(BroadcastEvent {
            user_id: user_id.to_string(),
            chat_id: None,
            space_id: None,
            kind: BroadcastEventKind::TaskUpdate {
                task_id: task_id.to_string(),
                status: status.to_string(),
                title: title.to_string(),
                chat_id: chat_id.map(|s| s.to_string()),
                source_chat_id: source_chat_id.map(|s| s.to_string()),
                result_summary: result_summary.map(|s| s.to_string()),
            },
        });
    }

    pub fn send_notification(&self, user_id: &str, notification: Notification) {
        self.dispatch(BroadcastEvent {
            user_id: user_id.to_string(),
            chat_id: None,
            space_id: None,
            kind: BroadcastEventKind::NewNotification { notification },
        });
    }

    pub fn broadcast_inference_count(&self, count: usize) {
        self.dispatch(BroadcastEvent {
            user_id: String::new(),
            chat_id: None,
            space_id: None,
            kind: BroadcastEventKind::InferenceCount { count },
        });
    }

    pub fn broadcast_activity_changed(&self, user_id: &str) {
        self.dispatch(BroadcastEvent {
            user_id: user_id.to_string(),
            chat_id: None,
            space_id: None,
            kind: BroadcastEventKind::ActivityChanged,
        });
    }

    pub fn broadcast_entity_updated(
        &self,
        user_id: &str,
        table: &str,
        record_id: &str,
        action: EntityAction,
        space_id: Option<String>,
        fields: Option<serde_json::Value>,
    ) {
        self.dispatch(BroadcastEvent {
            user_id: user_id.to_string(),
            chat_id: None,
            space_id: space_id.clone(),
            kind: BroadcastEventKind::EntityUpdated {
                table: table.to_string(),
                record_id: record_id.to_string(),
                action,
                space_id,
                fields,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_action_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&EntityAction::Created).unwrap(),
            "\"created\""
        );
        assert_eq!(
            serde_json::to_string(&EntityAction::Updated).unwrap(),
            "\"updated\""
        );
        assert_eq!(
            serde_json::to_string(&EntityAction::Deleted).unwrap(),
            "\"deleted\""
        );
    }

    #[test]
    fn entity_updated_event_maps_to_sse() {
        let event = BroadcastEvent {
            user_id: "u".to_string(),
            chat_id: None,
            space_id: None,
            kind: BroadcastEventKind::EntityUpdated {
                table: "space".to_string(),
                record_id: "s-1".to_string(),
                action: EntityAction::Updated,
                space_id: Some("s-1".to_string()),
                fields: Some(serde_json::json!({"channel:status": "connected"})),
            },
        };
        assert!(map_event_to_sse(&event).is_some());
    }

    #[test]
    fn entity_updated_without_space_id_maps_to_sse() {
        let event = BroadcastEvent {
            user_id: "u".to_string(),
            chat_id: None,
            space_id: None,
            kind: BroadcastEventKind::EntityUpdated {
                table: "agent".to_string(),
                record_id: "a-1".to_string(),
                action: EntityAction::Created,
                space_id: None,
                fields: None,
            },
        };
        assert!(map_event_to_sse(&event).is_some());
    }
}
