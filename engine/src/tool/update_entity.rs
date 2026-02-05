use async_trait::async_trait;
use serde_json::Value;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use surrealdb::types::RecordId;

use crate::error::AppError;
use crate::llm::tool_loop::{ToolLoopEvent, ToolLoopEventKind};

use super::{AgentTool, ToolContext, ToolDefinition, ToolOutput};

const PROTECTED_FIELDS: &[&str] = &["id", "user_id", "created_at"];

pub struct UpdateEntityTool {
    db: Surreal<Db>,
    user_id: String,
    table: String,
    record_id: String,
    tool_name: String,
}

impl UpdateEntityTool {
    pub fn new(
        db: Surreal<Db>,
        table: impl Into<String>,
        record_id: impl Into<String>,
        user_id: impl Into<String>,
        tool_name: impl Into<String>,
    ) -> Self {
        Self {
            db,
            user_id: user_id.into(),
            table: table.into(),
            record_id: record_id.into(),
            tool_name: tool_name.into(),
        }
    }
}

#[async_trait]
impl AgentTool for UpdateEntityTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: self.tool_name.clone(),
            description: format!(
                "Update fields on the current {}. Pass an object with the fields to update.",
                self.table
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "fields": {
                        "type": "object",
                        "description": "An object containing the fields to update and their new values"
                    }
                },
                "required": ["fields"]
            }),
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        tracing::debug!(
            table = %self.table,
            record_id = %self.record_id,
            arguments = %arguments,
            "UpdateEntityTool executing"
        );

        let mut fields = arguments
            .get("fields")
            .and_then(|v| v.as_object().cloned())
            .ok_or_else(|| AppError::Tool("'fields' must be a JSON object".into()))?;

        for key in PROTECTED_FIELDS {
            fields.remove(*key);
        }

        if fields.is_empty() {
            return Err(AppError::Tool("No updatable fields provided".into()));
        }

        let rid = RecordId::new(&*self.table, &*self.record_id);

        let query = format!(
            "SELECT user_id FROM {} WHERE id = $id LIMIT 1",
            self.table
        );
        let mut result = self
            .db
            .query(&query)
            .bind(("id", rid.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let record: Option<Value> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let record =
            record.ok_or_else(|| AppError::NotFound(format!("{} not found", self.table)))?;

        let owner = record.get("user_id").and_then(|v| v.as_str());

        if let Some(uid) = owner
            && uid != self.user_id
        {
            return Err(AppError::Forbidden("Not your record".into()));
        }

        fields.insert(
            "updated_at".to_string(),
            serde_json::json!(chrono::Utc::now()),
        );

        let field_names: Vec<String> = fields.keys().cloned().collect();
        let merge_value = Value::Object(fields);

        let update_query = format!("UPDATE {} MERGE $fields WHERE id = $id", self.table);
        self.db
            .query(&update_query)
            .bind(("id", rid))
            .bind(("fields", merge_value.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let _ = ctx
            .event_tx
            .send(ToolLoopEvent {
                kind: ToolLoopEventKind::EntityUpdated {
                    table: self.table.clone(),
                    record_id: self.record_id.clone(),
                    fields: merge_value,
                },
            })
            .await;

        Ok(ToolOutput::text(format!("Updated fields: {}", field_names.join(", "))))
    }
}
