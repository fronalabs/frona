//! Explicit bridge for runtime AgentTool implementations. This module owns the
//! identity dependency; the portable SDK tool module does not import it.

use async_trait::async_trait;
use frona_harness::{Tool, ToolContext, ToolError, ToolResult};
use rig_core::completion::request::ToolDefinition;
use serde_json::Value;

use crate::{core::error::AppError, inference::request::InferenceContext};

use super::{AgentTool, ToolOutput};

/// Bind one advertised runtime tool function and its real context. Output
/// mapping is deliberately explicit: HITL/task/attachment/system-prompt effects
/// belong to the runtime integration and must not silently become plain text.
pub struct AgentToolAdapter<'a, F> {
    tool: &'a dyn AgentTool,
    context: &'a InferenceContext,
    definition: ToolDefinition,
    map_output: F,
}

impl<'a, F: FnMut(ToolOutput) -> ToolResult + Send> AgentToolAdapter<'a, F> {
    pub fn new(
        tool: &'a dyn AgentTool,
        context: &'a InferenceContext,
        name: &str,
        map_output: F,
    ) -> Result<Self, AppError> {
        let definition = tool
            .definitions()
            .into_iter()
            .find(|definition| definition.id == name)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "tool '{}' does not advertise '{name}'",
                    tool.name()
                ))
            })?;
        Ok(Self {
            tool,
            context,
            definition: ToolDefinition {
                name: definition.id,
                description: definition.description,
                parameters: definition.parameters,
            },
            map_output,
        })
    }
}

#[async_trait]
impl<F: FnMut(ToolOutput) -> ToolResult + Send> Tool for AgentToolAdapter<'_, F> {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&mut self, arguments: Value, _context: &ToolContext) -> ToolResult {
        let output = self
            .tool
            .execute(&self.definition.name, arguments, self.context)
            .await
            .map_err(legacy_error)?;
        (self.map_output)(output)
    }
}

fn legacy_error(error: AppError) -> ToolError {
    fn terminal(error: &AppError) -> bool {
        match error {
            AppError::Inference(error) => error.is_terminal(),
            AppError::ToolExecution { source, .. } => terminal(source),
            _ => false,
        }
    }
    if !terminal(&error) {
        return ToolError::failed(error);
    }
    match error {
        AppError::Inference(error) => match error.into_execution_stop() {
            Ok(stop) => stop.into(),
            Err(error) => ToolError::failed(AppError::Inference(Box::new(error))),
        },
        AppError::ToolExecution { source, .. } => legacy_error(*source),
        error => ToolError::failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frona_harness::{ExecutionStop, inference::InferenceError};

    #[test]
    fn wrapped_cancellation_keeps_control_semantics_without_string_matching() {
        let error = AppError::ToolExecution {
            tool_name: "nested".into(),
            source: Box::new(AppError::Inference(Box::new(InferenceError::Cancelled(
                "user stopped".into(),
            )))),
        };
        assert!(
            matches!(legacy_error(error), ToolError::Stop(ExecutionStop::Cancelled(reason)) if reason == "user stopped")
        );
        assert!(matches!(
            legacy_error(AppError::Tool("cancelled is just text".into())),
            ToolError::Failed(_)
        ));
        for stop in [
            ExecutionStop::BudgetExhausted(frona_harness::BudgetRefusal::Denied("nested".into())),
            ExecutionStop::Storage("required history failed".into()),
        ] {
            let error = InferenceError::AllFallbacksFailed(vec![InferenceError::ModelFailed {
                provider: "test".into(),
                model: "test".into(),
                retry_count: 0,
                source: Box::new(InferenceError::Stop(stop)),
            }]);
            assert!(matches!(
                legacy_error(AppError::Inference(Box::new(error))),
                ToolError::Stop(_)
            ));
        }
        let wrapped = InferenceError::AllFallbacksFailed(vec![InferenceError::ModelFailed {
            provider: "test".into(),
            model: "test".into(),
            retry_count: 0,
            source: Box::new(InferenceError::Cancelled("nested cancellation".into())),
        }]);
        assert!(
            matches!(legacy_error(AppError::Inference(Box::new(wrapped))),
            ToolError::Stop(ExecutionStop::Cancelled(reason)) if reason == "nested cancellation")
        );
    }
}
