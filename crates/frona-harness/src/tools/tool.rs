//! Tool invocation contracts and typed results.

use async_trait::async_trait;
use rig_core::completion::request::ToolDefinition;
use serde::Serialize;
use serde_json::Value;

use super::ToolContext;
use crate::{ExecutionStop, ModelError, inference::InferenceError};

pub use rig_core::completion::message::ToolResultContent as ToolContent;

pub type ToolFailure = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool failed: {0}")]
    Failed(#[source] ToolFailure),
    /// Ordinary unsuccessful output retaining rich model content and metadata.
    #[error("tool returned unsuccessful output")]
    Feedback(ToolOutput),
    #[error(transparent)]
    Stop(#[from] ExecutionStop),
}

impl ToolError {
    /// Wrap an ordinary error without demoting an already typed SDK stop.
    pub fn failed(error: impl Into<ToolFailure>) -> Self {
        match error.into().downcast::<Self>() {
            Ok(error) => *error,
            Err(error) => match error.downcast::<ExecutionStop>() {
                Ok(stop) => Self::Stop(*stop),
                Err(error) => match error.downcast::<ModelError>() {
                    Ok(error) => Self::from(*error),
                    Err(error) => match error.downcast::<InferenceError>() {
                        Ok(error) => Self::from(*error),
                        Err(error) => Self::Failed(error),
                    },
                },
            },
        }
    }
}

impl From<InferenceError> for ToolError {
    fn from(error: InferenceError) -> Self {
        match error.into_execution_stop() {
            Ok(stop) => Self::Stop(stop),
            Err(error) => Self::Failed(Box::new(error)),
        }
    }
}

impl From<ModelError> for ToolError {
    fn from(error: ModelError) -> Self {
        match error {
            ModelError::Provider(error) => error.into(),
            error => Self::Failed(Box::new(error)),
        }
    }
}

pub type ToolResult = Result<ToolOutput, ToolError>;

/// Typed protocol content is retained unchanged, including image fields and
/// explicit JSON. Metadata is application-owned and is not model-facing text.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: Vec<ToolContent>,
    pub metadata: Value,
}

impl ToolOutput {
    pub fn content(content: Vec<ToolContent>) -> Self {
        Self {
            content,
            metadata: Value::Null,
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::content(vec![ToolContent::Text(text.into().into())])
    }

    pub fn json(value: impl Serialize) -> Result<Self, ToolError> {
        let value = serde_json::to_value(value).map_err(ToolError::failed)?;
        Ok(Self::content(vec![ToolContent::Json { value }]))
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Ordinary serializable results become explicit JSON; a rich ToolOutput keeps
/// its native content and metadata instead of being serialized a second time.
pub trait IntoToolOutput {
    fn into_tool_output(self) -> ToolResult;
}

impl<T: Serialize> IntoToolOutput for T {
    fn into_tool_output(self) -> ToolResult {
        ToolOutput::json(self)
    }
}

impl IntoToolOutput for ToolOutput {
    fn into_tool_output(self) -> ToolResult {
        Ok(self)
    }
}

/// A tool may borrow mutable application state. Invocation lends a Send future
/// from &mut self; neither Sync nor static state is required.
#[async_trait]
pub trait Tool: Send {
    fn definition(&self) -> &ToolDefinition;
    async fn call(&mut self, arguments: Value, context: &ToolContext<'_>) -> ToolResult;
}
