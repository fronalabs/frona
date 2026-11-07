//! Automatic orchestration over the same operations exposed by manual sessions.

use std::{future::IntoFuture, pin::Pin};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use super::{
    ActionKind, Conversation, ExecutionReport, ManualError, ManualSession, PendingAction, Run,
    Structured, Submission, Text,
};

/// An automatic run stopped without a completed output. The typed cause and
/// original conversation remain available for inspection; unfinished proposals
/// are not promoted into results. This does not imply durable resumability.
#[derive(thiserror::Error)]
#[error("run incomplete: {reason}")]
pub struct Incomplete<A = ()> {
    #[source]
    pub reason: ManualError,
    pub conversation: Box<Conversation>,
    /// Explicitly retained application progress, never an accepted result.
    pub retained: Option<A>,
    /// Final snapshot from the same recorder exposed by Run::report.
    pub report: Box<ExecutionReport>,
}

impl<A> std::fmt::Debug for Incomplete<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Incomplete")
            .field("reason", &self.reason)
            .field("conversation", &self.conversation)
            .field("has_retained", &self.retained.is_some())
            .field("report", &self.report)
            .finish()
    }
}

impl<'a> IntoFuture for Run<'a, Text> {
    type Output = Result<String, Incomplete>;
    type IntoFuture = Pin<Box<dyn std::future::Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let mut session = self.manual();
            drive_text(&mut session)
                .await
                .map_err(|reason| session.incomplete(reason, None))
        })
    }
}

impl<'a, T: DeserializeOwned + JsonSchema + 'a> IntoFuture for Run<'a, Structured<T>> {
    type Output = Result<T, Incomplete>;
    type IntoFuture = Pin<Box<dyn std::future::Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let mut session = self.manual();
            drive_structured(&mut session)
                .await
                .map_err(|reason| session.incomplete(reason, None))
        })
    }
}

async fn drive_text(session: &mut ManualSession<'_, Text>) -> Result<String, ManualError> {
    loop {
        let turn = session.call_model().await?;
        if turn.actions.is_empty() {
            return session.finish();
        }
        for action in turn.actions {
            match session.action(&action)?.kind {
                ActionKind::Tool => {
                    session.execute(action).await?;
                }
                ActionKind::Submission => {
                    session.reject(action, "Structured submission was not offered. Reply with your final answer as text.")?;
                }
            }
        }
    }
}

async fn drive_structured<T: DeserializeOwned + JsonSchema>(
    session: &mut ManualSession<'_, Structured<T>>,
) -> Result<T, ManualError> {
    let (action, value) = next_submission(session).await?;
    // Direct generation acknowledges schema completion, not domain acceptance.
    session.accept(action, value)
}

/// Shared exploration/repair policy for direct output and application inspection.
pub(super) async fn next_submission<T: DeserializeOwned + JsonSchema>(
    session: &mut ManualSession<'_, Structured<T>>,
) -> Result<(PendingAction, T), ManualError> {
    loop {
        let turn = session.call_model().await?;
        let mut submissions = Vec::new();
        // Finish sibling tool effects before choosing any output. A terminal
        // tool stop cannot turn an earlier valid-looking submission into success.
        for action in turn.actions {
            match session.action(&action)?.kind {
                ActionKind::Tool => {
                    session.execute(action).await?;
                }
                ActionKind::Submission => submissions.push(action),
            }
        }
        if submissions.is_empty() {
            session.repair_missing_submission()?;
            continue;
        }
        let mut selected = None;
        for action in submissions {
            if selected.is_some() {
                session.reject(action, "An earlier valid submission was selected. This additional submission was not selected.")?;
            } else if let Submission::Ready(value) = session.submission(&action)? {
                selected = Some((action, value));
            }
        }
        if let Some((action, value)) = selected {
            return Ok((action, value));
        }
    }
}
