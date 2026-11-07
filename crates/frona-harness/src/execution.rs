//! Admission and reliable completion at the actual provider invocation boundary.

use std::{future::Future, time::Instant};

use tokio_util::sync::CancellationToken;

use crate::inference::{
    InferenceError,
    observation::{AttemptOutcome, Observations},
    provider::ModelConfig,
};

use super::{
    Budget, BudgetCompletion, BudgetOperation, BudgetOutcome, ToolError, ToolResult, UsageReport,
};

/// Control stops must escape tool-error feedback and retry/fallback handling.
/// A storage stop refers to required execution persistence, not an ordinary
/// file operation performed by an application tool.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionStop {
    #[error("execution cancelled: {0}")]
    Cancelled(String),
    #[error("execution budget exhausted: {0}")]
    BudgetExhausted(super::BudgetRefusal),
    #[error("execution storage failed: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("execution stopped: {0}")]
    Stopped(String),
}

/// Borrowed controls shared by model operations, retries, and nested work.
/// Construction never creates or resets an allowance. Convenience model/group
/// methods create one default budget per logical invocation; loops should pass
/// the same budget here for every operation.
#[derive(Clone, Copy)]
pub struct ExecutionControl<'a> {
    budget: &'a dyn Budget,
    cancellation: Option<&'a CancellationToken>,
}

impl<'a> ExecutionControl<'a> {
    pub fn new(budget: &'a dyn Budget) -> Self {
        Self {
            budget,
            cancellation: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: &'a CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn budget(self) -> &'a dyn Budget {
        self.budget
    }

    pub fn is_cancelled(self) -> bool {
        self.cancellation
            .is_some_and(CancellationToken::is_cancelled)
    }

    pub(crate) fn check_cancellation(self) -> Result<(), InferenceError> {
        if self.is_cancelled() {
            Err(cancelled())
        } else {
            Ok(())
        }
    }

    /// Clone the caller's cancellation token for an owned asynchronous waiter.
    pub fn cancellation_token(self) -> Option<CancellationToken> {
        self.cancellation.cloned()
    }

    pub(crate) async fn cancelled(self) {
        match self.cancellation {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    }

    /// Shared per-tool admission. Call counts settle once; nested model usage
    /// is charged by its own model permits, never again by the enclosing tool.
    pub(crate) async fn tool_attempt<F, Fut>(self, name: &str, call: F) -> ToolResult
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ToolResult>,
    {
        self.check_cancellation()?;
        let permit = self
            .budget
            .admit(BudgetOperation::Tool { name })
            .map_err(ExecutionStop::BudgetExhausted)?;
        let result = tokio::select! {
            biased;
            _ = self.cancelled() => Err(ExecutionStop::Cancelled("execution cancelled".into()).into()),
            result = async { call().await } => result,
        };
        let result = if self.is_cancelled() {
            Err(ExecutionStop::Cancelled("execution cancelled".into()).into())
        } else {
            // Even callers constructing Failed directly cannot demote known
            // SDK control errors at the dispatch boundary.
            result.map_err(|error| match error {
                ToolError::Failed(error) => ToolError::failed(error),
                error => error,
            })
        };
        permit.complete(BudgetCompletion {
            outcome: match &result {
                Ok(_) => BudgetOutcome::Succeeded,
                Err(ToolError::Stop(ExecutionStop::Cancelled(_))) => BudgetOutcome::Cancelled,
                Err(_) => BudgetOutcome::Failed,
            },
            usage: UsageReport::Unavailable,
        });
        // Completion is application policy and may cancel the shared scope.
        // Keep its accounting, but never pair success after that cancellation.
        self.check_cancellation()?;
        result
    }

    /// The closure is not called on refusal. Accounting completes before
    /// best-effort telemetry. Cancellation drops in-flight provider work;
    /// budget exhaustion never revokes an admitted call. Dropping this future
    /// settles its permit as abandoned through the budget's own policy.
    pub async fn model_attempt<T, F, Fut>(
        self,
        model: &ModelConfig,
        observations: Observations<'_>,
        call: F,
        usage: impl Fn(&T) -> UsageReport,
    ) -> Result<T, InferenceError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, InferenceError>>,
    {
        self.check_cancellation()?;
        let permit = self
            .budget
            .admit(BudgetOperation::Model {
                provider: model.provider_name(),
                model: &model.model_id,
            })
            .map_err(ExecutionStop::BudgetExhausted)?;
        let start = Instant::now();
        let mut issued = false;
        let result = tokio::select! {
            biased;
            _ = self.cancelled() => Err(cancelled()),
            result = async { issued = true; call().await } => result,
        };
        let report = result
            .as_ref()
            .map(&usage)
            .unwrap_or(UsageReport::Unavailable);
        // A token may become cancelled during a synchronously-ready provider.
        // Preserve its returned usage while refusing to report successful work.
        let result = if self.is_cancelled() {
            Err(cancelled())
        } else {
            result
        };
        let outcome = match &result {
            Ok(_) => BudgetOutcome::Succeeded,
            Err(error) if error.is_cancelled() => BudgetOutcome::Cancelled,
            Err(_) => BudgetOutcome::Failed,
        };
        permit.complete(BudgetCompletion {
            outcome,
            usage: report.clone(),
        });
        if issued {
            observations
                .attempt(
                    model,
                    AttemptOutcome::from_result(&result),
                    report.observed(),
                    start.elapsed().as_millis() as u64,
                )
                .await;
        }
        self.check_cancellation()?;
        result
    }
}

fn cancelled() -> InferenceError {
    ExecutionStop::Cancelled("execution cancelled".into()).into()
}
