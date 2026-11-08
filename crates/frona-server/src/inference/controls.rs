//! Owned controls for one persistent runtime turn, including preparation.
//! Clones share consumption and evidence across session/agent reconstruction.

use super::InferenceResponse;
use crate::core::error::AppError;
use frona_harness::{
    Budget, DefaultBudget, ExecutionControl, FailureKind, ReportHandle, RunOutcome,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct RuntimeControls {
    budget: Arc<dyn Budget>,
    report: ReportHandle,
}

impl Default for RuntimeControls {
    fn default() -> Self {
        Self::new(Arc::new(DefaultBudget::default()))
    }
}

impl RuntimeControls {
    /// Supply one shared allowance for multiple turns; each controls object has
    /// its own report. Cloning this object preserves both allowance and report.
    pub fn new(budget: Arc<dyn Budget>) -> Self {
        Self {
            budget,
            report: ReportHandle::default(),
        }
    }
    pub fn report(&self) -> ReportHandle {
        self.report.clone()
    }
    pub fn execution<'a>(&'a self, cancellation: &'a CancellationToken) -> ExecutionControl<'a> {
        ExecutionControl::new(self.budget.as_ref())
            .with_report(&self.report)
            .with_cancellation(cancellation)
    }
}

pub(crate) fn outcome(outcome: Result<&InferenceResponse, &AppError>) -> RunOutcome {
    match outcome {
        Ok(InferenceResponse::Completed { .. } | InferenceResponse::Handled) => {
            RunOutcome::Completed
        }
        Ok(InferenceResponse::Shutdown { .. }) => {
            incomplete(FailureKind::ApplicationStop, "server shutting down")
        }
        Ok(InferenceResponse::Cancelled(_)) => {
            incomplete(FailureKind::Cancelled, "runtime cancelled")
        }
        Ok(InferenceResponse::Incomplete { reason, .. }) => reason.into(),
        Ok(InferenceResponse::ExternalToolPending { .. }) => incomplete(
            FailureKind::ApplicationStop,
            "waiting for external tool result",
        ),
        Err(AppError::Inference(error)) => error.as_ref().into(),
        Err(error) => incomplete(FailureKind::Setup, error.to_string()),
    }
}
fn incomplete(kind: FailureKind, message: impl Into<String>) -> RunOutcome {
    RunOutcome::Incomplete {
        kind,
        message: message.into(),
    }
}

/// Classify failure of a required canonical write, never ordinary tool I/O or reads.
pub(crate) fn required_write_error(error: AppError) -> AppError {
    AppError::Inference(Box::new(frona_harness::inference::InferenceError::Stop(
        frona_harness::ExecutionStop::Storage(Box::new(error)),
    )))
}
