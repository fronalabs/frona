use chrono::{DateTime, Utc};
use frona_derive::Entity;
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "skill")]
pub struct Skill {
    pub id: String,
    pub agent_id: Option<String>,
    pub name: String,
    pub description: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
