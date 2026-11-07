//! Admission and reliable completion at the actual provider invocation boundary.

use std::{future::Future, time::Instant};

use tokio_util::sync::CancellationToken;

use crate::inference::{
    InferenceError,
    observation::{AttemptOutcome, Observations},
    provider::ModelConfig,
};

use super::{
    ActionPosition, Budget, BudgetCompletion, BudgetOperation, BudgetOutcome, ReportHandle,
    RequestId, RequestInput, ToolError, ToolResult, UsageReport,
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

impl ExecutionStop {
    pub(super) fn failure_kind(&self) -> super::FailureKind {
        match self {
            Self::Cancelled(_) => super::FailureKind::Cancelled,
            Self::BudgetExhausted(_) => super::FailureKind::Budget,
            Self::Storage(_) => super::FailureKind::Storage,
            Self::Stopped(_) => super::FailureKind::ApplicationStop,
        }
    }
}

/// Borrowed controls shared by model operations, retries, and nested work.
/// Construction never creates or resets an allowance. Convenience model/group
/// methods create one default budget per logical invocation; loops should pass
/// the same budget here for every operation.
#[derive(Clone, Copy)]
pub struct ExecutionControl<'a> {
    budget: &'a dyn Budget,
    cancellation: Option<&'a CancellationToken>,
    report: Option<&'a ReportHandle>,
    request: Option<RequestId>,
    input: Option<&'a RequestInput>,
    action: Option<ActionPosition>,
}

impl<'a> ExecutionControl<'a> {
    pub fn new(budget: &'a dyn Budget) -> Self {
        Self {
            budget,
            cancellation: None,
            report: None,
            request: None,
            input: None,
            action: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: &'a CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn budget(self) -> &'a dyn Budget {
        self.budget
    }

    /// Attach reliable activity recording to low-level operations. ModelGroup
    /// retries share one logical request; nested calls create their own request.
    /// This does not finalize a run; configured SDK sessions own that lifecycle.
    pub fn with_report(mut self, report: &'a ReportHandle) -> Self {
        self.report = Some(report);
        self.request = None;
        self.input = None;
        self.action = None;
        self
    }

    pub(super) fn with_input(mut self, input: &'a RequestInput) -> Self {
        self.input = Some(input);
        self
    }

    pub(super) fn with_action(mut self, action: ActionPosition) -> Self {
        self.action = Some(action);
        self
    }

    pub(crate) fn begin_model_call(
        self,
        model: &ModelConfig,
    ) -> (Self, Option<super::report::RequestGuard>) {
        if self.request.is_some() {
            return (self, None);
        }
        let guard = self
            .report
            .map(|report| report.request(model.as_str(), self.input.cloned(), self.action));
        (
            Self {
                request: guard.as_ref().map(|guard| guard.id),
                ..self
            },
            guard,
        )
    }

    /// Record one logical model call while its controlled attempts run. The
    /// private request guard records completion or abandonment; this operation
    /// does not admit work itself. Invoke each provider through `model_attempt`.
    pub async fn model_call<T, F, Fut>(
        self,
        model: &ModelConfig,
        call: F,
    ) -> Result<T, InferenceError>
    where
        F: FnOnce(Self) -> Fut,
        Fut: Future<Output = Result<T, InferenceError>>,
    {
        let (execution, report) = self.begin_model_call(model);
        let result = call(execution).await;
        if let Some(report) = report {
            report.finish_inference(&result);
        }
        result
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
        let mut action = None;
        let result = tokio::select! {
            biased;
            _ = self.cancelled() => Err(ExecutionStop::Cancelled("execution cancelled".into()).into()),
            result = async {
                action = self.report.zip(self.action).map(|(report, position)| report.action_guard(position));
                call().await
            } => result,
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
        if let Some(action) = action {
            action.finish();
        }
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
        let mut attempt = None;
        let result = tokio::select! {
            biased;
            _ = self.cancelled() => Err(cancelled()),
            result = async {
                issued = true;
                attempt = self.report.zip(self.request).map(|(report, request)| report.attempt(request, model.as_str()));
                call().await
            } => result,
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
        let result = self.check_cancellation().and(result);
        if let Some(attempt) = attempt {
            attempt.finish(super::report::inference_outcome(&result), report.clone());
        }
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
