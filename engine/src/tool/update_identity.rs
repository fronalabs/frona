use std::collections::BTreeMap;

use serde_json::Value;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use surrealdb::types::RecordId;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::inference::tool_loop::{InferenceEvent, InferenceEventKind};
use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput};

pub struct UpdateIdentityTool {
    db: Surreal<Db>,
    agent_id: String,
    user_id: String,
    prompts: PromptLoader,
}

impl UpdateIdentityTool {
    pub fn new(
        db: Surreal<Db>,
        agent_id: impl Into<String>,
        user_id: impl Into<String>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            db,
            agent_id: agent_id.into(),
            user_id: user_id.into(),
            prompts,
        }
    }
}

#[agent_tool]
impl UpdateIdentityTool {
    async fn execute(&self, _tool_name: &str, arguments: Value, ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
        let attrs = arguments
            .get("attributes")
            .and_then(|v| v.as_object().cloned())
            .ok_or_else(|| AppError::Tool("'attributes' must be a JSON object".into()))?;

        if attrs.is_empty() {
            return Err(AppError::Tool("No attributes provided".into()));
        }

        let rid = RecordId::new("agent", &*self.agent_id);

        let mut result = self
            .db
            .query("SELECT identity, name, user_id FROM agent WHERE id = $id LIMIT 1")
            .bind(("id", rid.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let record: Option<Value> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let record = record.ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        let owner = record.get("user_id").and_then(|v| v.as_str());
        if let Some(uid) = owner
            && uid != self.user_id
        {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        let mut existing: BTreeMap<String, String> = record
            .get("identity")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let mut new_name: Option<String> = None;

        for (key, value) in &attrs {
            let val_str = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };

            if val_str.is_empty() {
                existing.remove(key);
            } else {
                if key.eq_ignore_ascii_case("name") {
                    new_name = Some(val_str.clone());
                }
                existing.insert(key.clone(), val_str);
            }
        }

        let current_name = record
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let agent_name = new_name.clone().unwrap_or(current_name);

        self.db
            .query("UPDATE agent SET identity = $identity, name = $name, updated_at = $now WHERE id = $id")
            .bind(("id", rid))
            .bind(("identity", serde_json::to_value(&existing).unwrap()))
            .bind(("name", agent_name.clone()))
            .bind(("now", chrono::Utc::now()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut entity_fields = serde_json::Map::new();
        entity_fields.insert("identity".to_string(), serde_json::to_value(&existing).unwrap());
        if new_name.is_some() {
            entity_fields.insert("name".to_string(), serde_json::json!(agent_name));
        }

        let _ = ctx
            .event_tx
            .send(InferenceEvent {
                kind: InferenceEventKind::EntityUpdated {
                    table: "agent".to_string(),
                    record_id: self.agent_id.clone(),
                    fields: Value::Object(entity_fields),
                },
            });

        let updated_keys: Vec<&String> = attrs.keys().collect();
        Ok(ToolOutput::text(format!(
            "Updated identity attributes: {}",
            updated_keys
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}
