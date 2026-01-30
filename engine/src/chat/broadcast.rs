use std::sync::Arc;

use tokio::sync::broadcast;

use super::message::dto::MessageResponse;

#[derive(Debug, Clone)]
pub enum BroadcastEvent {
    ChatMessage {
        user_id: String,
        chat_id: String,
        message: MessageResponse,
    },
}

#[derive(Clone)]
pub struct BroadcastService {
    tx: Arc<broadcast::Sender<BroadcastEvent>>,
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

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastEvent> {
        self.tx.subscribe()
    }
}
