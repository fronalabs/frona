//! Call-local identity, cancellation and shared execution controls.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::ToolError;
use crate::inference::{observation::Observations, provider::StreamToken};
use crate::{ExecutionControl, ExecutionStop, Model, ModelOutput, ModelRequest};

/// Call-local information with no persistent identity or services. Dispatch
/// attaches the caller's budget; nested model work never creates a new allowance.
/// A default context supports direct low-level tools, but nested work requires
/// a dispatcher-supplied execution control.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct ToolContext<'a> {
    pub call_id: Option<String>,
    pub cancellation: CancellationToken,
    execution: Option<ExecutionControl<'a>>,
}

impl std::fmt::Debug for ToolContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("call_id", &self.call_id)
            .field("cancellation", &self.cancellation)
            .field("has_execution", &self.execution.is_some())
            .finish()
    }
}

impl<'a> ToolContext<'a> {
    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub(crate) fn for_execution(call_id: String, execution: ExecutionControl<'a>) -> Self {
        Self {
            call_id: Some(call_id),
            cancellation: execution.cancellation_token().unwrap_or_default(),
            execution: Some(execution),
        }
    }

    /// Reuse this tool's exact allowance and cancellation for a nested model.
    /// Errors retain typed control stops and can be propagated with `?`.
    pub async fn complete(
        &self,
        model: &Model<'_>,
        request: ModelRequest,
    ) -> Result<ModelOutput, ToolError> {
        self.invoke(model, request, None, Observations::default())
            .await
    }

    pub async fn invoke(
        &self,
        model: &Model<'_>,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
        observations: Observations<'_>,
    ) -> Result<ModelOutput, ToolError> {
        model
            .invoke_controlled(request, tokens, observations, self.execution()?)
            .await
            .map_err(Into::into)
    }

    /// Borrow the same controls for other budget-aware SDK operations.
    pub fn execution(&self) -> Result<ExecutionControl<'_>, ToolError> {
        let execution = self.execution.ok_or_else(|| {
            ExecutionStop::Stopped(
                "nested model work requires a dispatcher execution context".into(),
            )
        })?;
        Ok(execution.with_cancellation(&self.cancellation))
    }
}
