use chrono::{DateTime, Utc};
use frona_derive::Entity;
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "prompt")]
pub struct Prompt {
    pub id: String,
    pub agent_id: String,
    pub name: String,
    pub template: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
