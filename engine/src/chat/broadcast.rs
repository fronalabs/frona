use std::sync::Arc;

use tokio::sync::broadcast;

use super::message::models::MessageResponse;

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum BroadcastEvent {
    ChatMessage {
        user_id: String,
        chat_id: String,
        message: MessageResponse,
    },
    TaskUpdate {
        user_id: String,
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
}

#[derive(Clone)]
pub struct BroadcastService {
    tx: Arc<broadcast::Sender<BroadcastEvent>>,
}

impl Default for BroadcastService {
    fn default() -> Self {
        Self::new()
    }
}

impl BroadcastService {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx: Arc::new(tx) }
    }

    pub fn broadcast_chat_message(
        &self,
        user_id: &str,
        chat_id: &str,
        message: MessageResponse,
    ) {
        let _ = self.tx.send(BroadcastEvent::ChatMessage {
            user_id: user_id.to_string(),
            chat_id: chat_id.to_string(),
            message,
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
        let _ = self.tx.send(BroadcastEvent::TaskUpdate {
            user_id: user_id.to_string(),
            task_id: task_id.to_string(),
            status: status.to_string(),
            title: title.to_string(),
            chat_id: chat_id.map(|s| s.to_string()),
            source_chat_id: source_chat_id.map(|s| s.to_string()),
            result_summary: result_summary.map(|s| s.to_string()),
        });
    }

    pub fn broadcast_inference_count(&self, count: usize) {
        let _ = self.tx.send(BroadcastEvent::InferenceCount { count });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastEvent> {
        self.tx.subscribe()
    }
}
