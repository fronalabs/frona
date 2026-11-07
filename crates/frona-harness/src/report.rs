//! Reliable in-process execution evidence. This is not an event queue or transcript.

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use super::{ActionKind, ActionResolution, BudgetOutcome, ExecutionStop, ManualError, UsageReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptId(pub usize);

/// Internal action identity, independent of possibly duplicated provider call IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionPosition {
    pub conversation: Uuid,
    pub response: usize,
    pub action: usize,
}

/// Zero-based original conversation message positions included in this request.
/// Recording inclusion proves dispatch, not remote comprehension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestInput {
    pub conversation: Uuid,
    pub messages: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Abandoned,
}

pub(crate) fn inference_outcome<T>(
    result: &Result<T, crate::inference::InferenceError>,
) -> CallOutcome {
    match result {
        Ok(_) => CallOutcome::Succeeded,
        Err(error) if error.is_cancelled() => CallOutcome::Cancelled,
        Err(_) => CallOutcome::Failed,
    }
}

impl From<BudgetOutcome> for CallOutcome {
    fn from(value: BudgetOutcome) -> Self {
        match value {
            BudgetOutcome::Succeeded => Self::Succeeded,
            BudgetOutcome::Failed => Self::Failed,
            BudgetOutcome::Cancelled => Self::Cancelled,
            BudgetOutcome::Abandoned => Self::Abandoned,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestReport {
    pub id: RequestId,
    pub model: String,
    /// None for nested or standalone low-level calls without conversation mapping.
    pub input: Option<RequestInput>,
    pub parent_action: Option<ActionPosition>,
    pub outcome: Option<CallOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptReport {
    pub id: AttemptId,
    pub request: RequestId,
    /// The actual candidate, which may differ after fallback.
    pub model: String,
    pub outcome: Option<CallOutcome>,
    /// None while in flight; Unavailable on completion is distinct from zero.
    pub usage: Option<UsageReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Resolved,
    Failed,
    Rejected,
    Stopped,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionReport {
    pub position: ActionPosition,
    pub call_id: String,
    pub name: String,
    pub kind: ActionKind,
    pub started: bool,
    pub outcome: Option<ActionOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackReport {
    pub conversation: Uuid,
    /// Present for paired results; absent for ordinary user feedback/repair.
    pub action: Option<ActionPosition>,
    /// None while a partially resolved result batch is not yet replayable.
    pub message: Option<usize>,
    pub issued_in: Vec<AttemptId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Budget,
    Cancelled,
    Storage,
    ApplicationStop,
    Inspector,
    ToolHook,
    Model,
    Setup,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Incomplete {
        kind: FailureKind,
        message: String,
    },
    /// The run/session was dropped without explicit completion, including an
    /// unpolled run or a dropped future. This never implies successful effects.
    Abandoned,
}

impl RunOutcome {
    fn incomplete(error: &ManualError) -> Self {
        let kind = match error {
            ManualError::Stop(ExecutionStop::BudgetExhausted(_)) => FailureKind::Budget,
            ManualError::Stop(ExecutionStop::Cancelled(_)) => FailureKind::Cancelled,
            ManualError::Stop(ExecutionStop::Storage(_)) => FailureKind::Storage,
            ManualError::Stop(ExecutionStop::Stopped(_)) => FailureKind::ApplicationStop,
            ManualError::Inspector(_) => FailureKind::Inspector,
            ManualError::ToolHook(_) => FailureKind::ToolHook,
            ManualError::Model(_) => FailureKind::Model,
            ManualError::Setup(_) => FailureKind::Setup,
            _ => FailureKind::Protocol,
        };
        Self::Incomplete {
            kind,
            message: error.to_string(),
        }
    }
}

/// A snapshot contains activity metadata and positions, not message bodies or
/// application retained values. Its size grows with recorded execution activity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionReport {
    pub requests: Vec<RequestReport>,
    pub attempts: Vec<AttemptReport>,
    pub actions: Vec<ActionReport>,
    pub feedback: Vec<FeedbackReport>,
    pub outcome: Option<RunOutcome>,
}

impl ExecutionReport {
    pub fn logical_calls(&self) -> usize {
        self.requests.len()
    }
    pub fn provider_attempts(&self) -> usize {
        self.attempts.len()
    }
}

/// Independent read-only access to one recorder. Holding or reading this handle
/// never drives execution and requires no subscriber. Run::report returns it.
/// Default creates a detached recorder for low-level controlled operations;
/// those operations record calls but do not own/finalize a run lifecycle.
#[derive(Debug, Clone, Default)]
pub struct ReportHandle(Arc<Mutex<ExecutionReport>>);

impl ReportHandle {
    pub fn snapshot(&self) -> ExecutionReport {
        self.read().clone()
    }

    fn read(&self) -> std::sync::MutexGuard<'_, ExecutionReport> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn request(
        &self,
        model: String,
        input: Option<RequestInput>,
        parent_action: Option<ActionPosition>,
    ) -> RequestGuard {
        let mut report = self.read();
        let id = RequestId(report.requests.len());
        report.requests.push(RequestReport {
            id,
            model,
            input,
            parent_action,
            outcome: None,
        });
        RequestGuard {
            report: self.clone(),
            id,
            complete: false,
        }
    }

    pub(super) fn attempt(&self, request: RequestId, model: String) -> AttemptGuard {
        let mut report = self.read();
        let id = AttemptId(report.attempts.len());
        let input = report.requests[request.0].input.clone();
        if let Some(input) = input {
            for feedback in &mut report.feedback {
                if feedback.conversation == input.conversation
                    && feedback
                        .message
                        .is_some_and(|position| input.messages.contains(&position))
                {
                    feedback.issued_in.push(id);
                }
            }
        }
        report.attempts.push(AttemptReport {
            id,
            request,
            model,
            outcome: None,
            usage: None,
        });
        AttemptGuard {
            report: self.clone(),
            id,
            complete: false,
        }
    }

    pub(super) fn proposed(
        &self,
        position: ActionPosition,
        call: &rig_core::completion::message::ToolCall,
        kind: ActionKind,
    ) {
        self.read().actions.push(ActionReport {
            position,
            call_id: call.id.as_str().into(),
            name: call.function.name.clone(),
            kind,
            started: false,
            outcome: None,
        });
    }

    pub(super) fn started(&self, position: ActionPosition) {
        if let Some(action) = self
            .read()
            .actions
            .iter_mut()
            .find(|a| a.position == position)
        {
            action.started = true;
        }
    }

    pub(super) fn action_guard(&self, position: ActionPosition) -> ActionGuard {
        ActionGuard {
            report: self.clone(),
            position,
            complete: false,
        }
    }

    pub(super) fn resolved(&self, position: ActionPosition, resolution: &ActionResolution) {
        let mut report = self.read();
        if let Some(action) = report.actions.iter_mut().find(|a| a.position == position) {
            action.outcome = Some(match resolution {
                ActionResolution::Result(_) => ActionOutcome::Resolved,
                ActionResolution::Failed(_) => ActionOutcome::Failed,
                ActionResolution::Rejected(_) => ActionOutcome::Rejected,
            });
        }
        report.feedback.push(FeedbackReport {
            conversation: position.conversation,
            action: Some(position),
            message: None,
            issued_in: Vec::new(),
        });
    }

    pub(super) fn message(&self, conversation: Uuid, message: usize, response: Option<usize>) {
        let mut report = self.read();
        if let Some(response) = response {
            for feedback in &mut report.feedback {
                if feedback
                    .action
                    .is_some_and(|p| p.conversation == conversation && p.response == response)
                {
                    feedback.message = Some(message);
                }
            }
        } else {
            report.feedback.push(FeedbackReport {
                conversation,
                action: None,
                message: Some(message),
                issued_in: Vec::new(),
            });
        }
    }

    pub(super) fn stopped_actions(&self, conversation: Uuid) {
        for action in &mut self.read().actions {
            if action.position.conversation == conversation
                && action.started
                && action.outcome.is_none()
            {
                action.outcome = Some(ActionOutcome::Stopped);
            }
        }
    }

    pub(super) fn finish(&self, outcome: RunOutcome) {
        let mut report = self.read();
        if report.outcome.is_none() {
            report.outcome = Some(outcome);
        }
    }
}

pub(crate) struct RequestGuard {
    report: ReportHandle,
    pub id: RequestId,
    complete: bool,
}
impl RequestGuard {
    pub fn finish_inference<T>(self, result: &Result<T, crate::inference::InferenceError>) {
        self.finish(inference_outcome(result));
    }
    pub fn finish(mut self, outcome: CallOutcome) {
        self.report.read().requests[self.id.0].outcome = Some(outcome);
        self.complete = true;
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.report.read().requests[self.id.0].outcome = Some(CallOutcome::Abandoned);
        }
    }
}

pub(super) struct AttemptGuard {
    report: ReportHandle,
    id: AttemptId,
    complete: bool,
}
impl AttemptGuard {
    pub fn finish(mut self, outcome: CallOutcome, usage: UsageReport) {
        let mut report = self.report.read();
        let attempt = &mut report.attempts[self.id.0];
        attempt.outcome = Some(outcome);
        attempt.usage = Some(usage);
        self.complete = true;
    }
}

pub(super) struct ActionGuard {
    report: ReportHandle,
    position: ActionPosition,
    complete: bool,
}
impl ActionGuard {
    pub fn finish(mut self) {
        self.complete = true;
    }
}
impl Drop for ActionGuard {
    fn drop(&mut self) {
        if !self.complete
            && let Some(action) = self
                .report
                .read()
                .actions
                .iter_mut()
                .find(|a| a.position == self.position)
        {
            action.outcome = Some(ActionOutcome::Abandoned);
        }
    }
}
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if !self.complete {
            let mut report = self.report.read();
            let attempt = &mut report.attempts[self.id.0];
            attempt.outcome = Some(CallOutcome::Abandoned);
            attempt.usage = Some(UsageReport::Unavailable);
        }
    }
}

/// Only the session owns lifecycle finalization; reader clones cannot keep a
/// dropped run marked active. Recorders never call application code under locks.
pub(super) struct RunRecorder(pub ReportHandle);
impl RunRecorder {
    pub fn new() -> Self {
        Self(ReportHandle::default())
    }
    pub fn incomplete(&self, error: &ManualError) {
        self.0.finish(RunOutcome::incomplete(error));
    }
}
impl Drop for RunRecorder {
    fn drop(&mut self) {
        self.0.finish(RunOutcome::Abandoned);
    }
}
