use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::task::executor::{TaskLifecycleEvent, deliver_event_to_source};
use crate::agent::task::models::{SignalMode, TaskKind};
use crate::agent::task::schema::ResultSpec;
use crate::chat::service::ChatService;
use crate::core::error::AppError;
use frona_text::prompt::PromptLoader;

use super::{AgentTool, InferenceContext, ToolDefinition, ToolOutput, load_tool_definition};

const MAX_SUMMARY_LEN: usize = 512;

pub struct ReportSignalTool {
    chat_service: ChatService,
    prompts: PromptLoader,
    result_schema: Option<Arc<ResultSpec>>,
}

impl ReportSignalTool {
    pub fn new(
        chat_service: ChatService,
        prompts: PromptLoader,
        result_schema: Option<Arc<ResultSpec>>,
    ) -> Self {
        Self {
            chat_service,
            prompts,
            result_schema,
        }
    }
}

#[async_trait]
impl AgentTool for ReportSignalTool {
    fn name(&self) -> &str {
        "report_signal"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        let def = load_tool_definition(&self.prompts, "tools/report_signal.md").map(|mut def| {
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
        def.map(|d| vec![d]).unwrap_or_default()
    }

    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let task = ctx.task.as_ref().ok_or_else(|| {
            AppError::Tool("report_signal can only be used within a task context".into())
        })?;

        let attempt_index = match &task.kind {
            TaskKind::Signal {
                mode: SignalMode::Continuous,
                evaluation_count,
                ..
            } => *evaluation_count,
            _ => {
                return Err(AppError::Validation(
                    "report_signal is only available for continuous signal tasks".into(),
                ));
            }
        };

        let summary = arguments
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .ok_or_else(|| AppError::Validation("Missing required parameter: summary".into()))?;
        if summary.is_empty() {
            return Err(AppError::Validation("summary must not be empty".into()));
        }
        if summary.len() > MAX_SUMMARY_LEN {
            return Err(AppError::Validation(format!(
                "summary must be at most {MAX_SUMMARY_LEN} bytes"
            )));
        }

        let result = arguments.get("result").cloned();
        if let (Some(spec), Some(value)) = (self.result_schema.as_ref(), result.as_ref()) {
            let serialized = match value {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).map_err(|e| {
                    AppError::Internal(format!("failed to serialize result for validation: {e}"))
                })?,
            };
            spec.validate(&serialized).map_err(|reason| {
                AppError::Validation(format!(
                    "result does not match the task's declared schema: {reason}"
                ))
            })?;
        }

        deliver_event_to_source(
            &self.chat_service,
            task,
            TaskLifecycleEvent::Match {
                attempt_index,
                summary,
                result,
            },
            vec![],
        )
        .await;

        Ok(ToolOutput::text("Match recorded. Watch is still active."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_summary_len_is_512() {
        assert_eq!(MAX_SUMMARY_LEN, 512);
    }
}
