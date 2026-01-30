use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateChatRequest {
    pub space_id: Option<String>,
    pub agent_id: String,
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateChatRequest {
    pub title: Option<String>,
    pub space_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub id: String,
    pub space_id: Option<String>,
    pub agent_id: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<super::models::Chat> for ChatResponse {
    fn from(chat: super::models::Chat) -> Self {
        Self {
            id: chat.id,
            space_id: chat.space_id,
            agent_id: chat.agent_id,
            title: chat.title,
            created_at: chat.created_at,
            updated_at: chat.updated_at,
        }
    }
}
