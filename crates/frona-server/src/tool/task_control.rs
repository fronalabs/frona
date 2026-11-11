use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::task::models::TaskStatus;
use crate::agent::task::schema::ResultSpec;
use crate::core::error::AppError;
use crate::inference::tool_call::TaskEvent;
use crate::storage::resolve_workspace_attachment;
use crate::storage::service::StorageService;
use frona_text::prompt::PromptLoader;

use super::{
    AgentTool, InferenceContext, ToolDefinition, ToolOutput, active_chat, load_tool_definition,
};

pub struct TaskControlTool {
    storage: StorageService,
    prompts: PromptLoader,
    result_schema: Option<Arc<ResultSpec>>,
}

impl TaskControlTool {
    pub fn new(
        storage: StorageService,
        prompts: PromptLoader,
        result_schema: Option<Arc<ResultSpec>>,
    ) -> Self {
        Self {
            storage,
            prompts,
            result_schema,
        }
    }
}

#[async_trait]
impl AgentTool for TaskControlTool {
    fn name(&self) -> &str {
        "task_control"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        let complete =
            load_tool_definition(&self.prompts, "tools/complete_task.md").map(|mut def| {
                if let Some(spec) = &self.result_schema
                    && let Some(props) = def
                        .parameters
                        .as_object_mut()
                        .and_then(|o| o.get_mut("properties"))
                        .and_then(|p| p.as_object_mut())
                {
                    props.insert("result".to_string(), spec.schema.clone());
                }
                def
            });
        let fail = load_tool_definition(&self.prompts, "tools/fail_task.md");
        let defer = load_tool_definition(&self.prompts, "tools/defer_task.md");
        [complete, fail, defer].into_iter().flatten().collect()
    }

    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let task = ctx.task.as_ref().ok_or_else(|| {
            AppError::Tool("task_control tools can only be used within a task context".into())
        })?;
        let chat = active_chat(ctx)?;

        match tool_name {
            "complete_task" => {
                // `as_str` was load-bearing wrong: agents pass numbers, objects,
                // arrays, or null and that silently dropped them to None.
                let result_value = arguments.get("result").cloned();

                let result: Option<String> = if let Some(spec) = self.result_schema.as_ref() {
                    let schema_type = spec.schema.get("type").and_then(|t| t.as_str());
                    // Legacy stringified-JSON fallback for older agent calls.
                    let validation_value = match (&result_value, schema_type) {
                        (Some(Value::String(s)), Some(t)) if t != "string" => {
                            serde_json::from_str::<Value>(s)
                                .unwrap_or_else(|_| Value::String(s.clone()))
                        }
                        (Some(v), _) => v.clone(),
                        // Missing => null forces a tool error against non-nullable schemas.
                        (None, _) => Value::Null,
                    };
                    if let Err(reason) = spec.validate_value(&validation_value) {
                        return Err(AppError::Validation(format!(
                            "result does not match the task's declared schema: {reason}"
                        )));
                    }
                    // Storage must be roundtrippable by `ResultSpec::parse`,
                    // hence the type=string asymmetry (raw vs JSON-encoded).
                    if result_value.is_none() {
                        None
                    } else {
                        Some(match (&validation_value, schema_type) {
                            (Value::String(s), Some("string")) => s.clone(),
                            _ => serde_json::to_string(&validation_value).unwrap_or_default(),
                        })
                    }
                } else {
                    result_value
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                };

                let mut resolved_deliverables = Vec::new();
                if let Some(deliverables) = arguments.get("deliverables").and_then(|v| v.as_array())
                {
                    for path_val in deliverables {
                        if let Some(path) = path_val.as_str() {
                            let attachment = resolve_workspace_attachment(
                                &self.storage,
                                &ctx.user.handle,
                                &ctx.agent.handle,
                                path,
                            )
                            .await?;
                            resolved_deliverables.push(attachment);
                        }
                    }
                }

                let mut output = ToolOutput::text("Task marked as complete.").with_task_event(
                    TaskEvent::Completion {
                        task_id: task.id.clone(),
                        chat_id: Some(chat.id.clone()),
                        status: TaskStatus::Completed,
                        summary: result,
                        deliverables: resolved_deliverables.clone(),
                    },
                );

                for attachment in resolved_deliverables {
                    output = output.with_attachment(attachment);
                }

                Ok(output)
            }
            "fail_task" => {
                let reason = arguments
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AppError::Validation("Missing 'reason' parameter".into()))?;

                Ok(ToolOutput::text("Task marked as failed.").with_task_event(
                    TaskEvent::Completion {
                        task_id: task.id.clone(),
                        chat_id: Some(chat.id.clone()),
                        status: TaskStatus::Failed,
                        summary: Some(reason.to_string()),
                        deliverables: vec![],
                    },
                ))
            }
            "defer_task" => {
                let delay_minutes = arguments
                    .get("delay_minutes")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        AppError::Validation("Missing 'delay_minutes' parameter".into())
                    })? as u32;

                let reason = arguments
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AppError::Validation("Missing 'reason' parameter".into()))?;

                Ok(
                    ToolOutput::text(format!("Task deferred for {delay_minutes} minutes."))
                        .with_task_event(TaskEvent::Deferred {
                            task_id: task.id.clone(),
                            delay_minutes,
                            reason: reason.to_string(),
                        }),
                )
            }
            _ => Err(AppError::Tool(format!(
                "Unknown task_control tool: {tool_name}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_with_schema(schema: Option<Value>) -> TaskControlTool {
        let prompts = PromptLoader::new(
            std::env::current_dir()
                .unwrap()
                .ancestors()
                .find(|p| p.join("resources/prompts").exists())
                .unwrap()
                .join("resources/prompts"),
        );
        let spec = schema.map(|s| Arc::new(ResultSpec::new(s).expect("valid schema")));
        TaskControlTool::new(
            StorageService::new(&crate::core::config::Config::default()),
            prompts,
            spec,
        )
    }

    #[test]
    fn definitions_include_complete_fail_defer() {
        let tool = tool_with_schema(None);
        let defs = tool.definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.id.as_str()).collect();
        assert!(
            names.contains(&"complete_task"),
            "missing complete_task: {names:?}"
        );
        assert!(names.contains(&"fail_task"), "missing fail_task: {names:?}");
        assert!(
            names.contains(&"defer_task"),
            "missing defer_task: {names:?}"
        );
    }

    #[test]
    fn definitions_patch_complete_result_when_schema_present() {
        let schema = json!({"type": "string", "pattern": "^[0-9]{6}$"});
        let tool = tool_with_schema(Some(schema.clone()));
        let defs = tool.definitions();
        let complete = defs
            .iter()
            .find(|d| d.id == "complete_task")
            .expect("complete_task definition");
        let result_schema = complete
            .parameters
            .get("properties")
            .and_then(|p| p.get("result"))
            .expect("complete_task.parameters.properties.result");
        assert_eq!(result_schema, &schema);
    }

    #[test]
    fn definitions_leave_complete_result_unchanged_when_no_schema() {
        let tool = tool_with_schema(None);
        let defs = tool.definitions();
        let complete = defs
            .iter()
            .find(|d| d.id == "complete_task")
            .expect("complete_task definition");
        let result_schema = complete
            .parameters
            .get("properties")
            .and_then(|p| p.get("result"))
            .expect("complete_task.parameters.properties.result");
        assert_ne!(
            result_schema.get("pattern"),
            Some(&json!("^[0-9]{6}$")),
            "result schema should not have been patched"
        );
    }
}
