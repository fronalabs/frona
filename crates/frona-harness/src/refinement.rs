//! Application inspection over the shared submission and execution operations.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use super::{
    ExecutionStop, Incomplete, ManualError, RefinementStage, Run, Structured,
    automatic::next_submission,
};

/// An explicit application decision. Retention preserves unfinished progress;
/// only `accept` can return a successful refinement result.
#[derive(Debug)]
pub enum Decision<A> {
    Accept {
        value: A,
        retained: Option<A>,
    },
    Revise {
        feedback: String,
        retained: Option<A>,
    },
    Abandon {
        reason: String,
        retained: Option<A>,
    },
    Stop {
        reason: String,
        retained: Option<A>,
    },
}

impl<A> Decision<A> {
    pub fn accept(value: A) -> Self {
        Self::Accept {
            value,
            retained: None,
        }
    }

    pub fn revise(feedback: impl Into<String>) -> Self {
        Self::Revise {
            feedback: feedback.into(),
            retained: None,
        }
    }

    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop {
            reason: reason.into(),
            retained: None,
        }
    }

    /// Explicitly abandon this refinement without accepting or discarding retained work.
    /// Distinct from dropping a future, which reports RunOutcome::Abandoned.
    pub fn abandon(reason: impl Into<String>) -> Self {
        Self::Abandon {
            reason: reason.into(),
            retained: None,
        }
    }

    /// Explicitly replace retained progress. Omission preserves the prior value;
    /// ranking is application policy. Retention does not alter the decision.
    /// With `accept`, this value matters only if completion is interrupted.
    pub fn retain(mut self, value: A) -> Self {
        *self.retained_mut() = Some(value);
        self
    }

    fn retained_mut(&mut self) -> &mut Option<A> {
        match self {
            Self::Accept { retained, .. }
            | Self::Revise { retained, .. }
            | Self::Stop { retained, .. }
            | Self::Abandon { retained, .. } => retained,
        }
    }
}

impl<T: DeserializeOwned + JsonSchema> Run<'_, Structured<T>> {
    /// Inspect typed submissions sequentially. The callback can lend mutable
    /// application state across await, including local non-Send state. No task is
    /// spawned; the resulting future is Send when its actual captures permit it.
    /// Inspector errors are diagnostic application failures, never model feedback.
    pub async fn inspect<A, E: std::fmt::Debug, F>(
        self,
        mut inspector: F,
    ) -> Result<A, Incomplete<A>>
    where
        F: AsyncFnMut(T) -> Result<Decision<A>, E>,
    {
        let mut session = self.manual();
        let mut retained = None;
        let outcome = loop {
            let (action, value) = match next_submission(&mut session).await {
                Ok(submission) => submission,
                Err(error) => break Err(error),
            };
            let cancellation = session.cancellation_token();
            session.refinement_progress(RefinementStage::Inspecting);
            let mut decision = tokio::select! {
                biased;
                _ = cancellation.cancelled() => break Err(ManualError::Stop(
                    ExecutionStop::Cancelled("execution cancelled during inspection".into())
                )),
                result = inspector(value) => match result {
                    Ok(decision) => decision,
                    Err(error) => break Err(ManualError::Inspector(format!("{error:?}"))),
                },
            };
            if let Some(candidate) = decision.retained_mut().take() {
                retained = Some(candidate);
                session.refinement_progress(RefinementStage::Retained);
            }
            match decision {
                Decision::Accept { value, .. } => break session.accept(action, value),
                Decision::Revise { feedback, .. } => {
                    if let Err(error) = session.revise(action, feedback) {
                        break Err(error);
                    }
                }
                Decision::Stop { reason, .. } => break Err(session.stop(reason)),
                Decision::Abandon { reason, .. } => break Err(ManualError::Abandoned(reason)),
            }
        };
        outcome.map_err(|reason| session.incomplete(reason, retained))
    }
}
