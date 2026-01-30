use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use frona_derive::Entity;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "fact")]
pub struct Fact {
    pub id: String,
    pub agent_id: String,
    pub content: String,
    pub source_chat_id: Option<String>,
    pub created_at: DateTime<Utc>,
}
