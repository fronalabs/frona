use crate::model::KnowledgeMemory;
use serde_json::json;

pub fn memory(id: &str) -> KnowledgeMemory {
    serde_json::from_value(json!({"id":id,"user_id":"user","created_at":"2026-11-11T00:00:00Z",
        "kind":"fact","content":"Postgres runs on 5433.","relations":[],"disposition":"none","evidence":[]})).unwrap()
}
