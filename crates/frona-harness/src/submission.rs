//! Typed submission inspection and paired repair without provider dispatch.

use rig_core::completion::{message::UserContent, request::ToolDefinition};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use super::{
    ActionKind, ActionResolution, ActionStatus, Conversation, ConversationError, PendingAction,
    ToolOutput,
};
use crate::inference::{provider::SUBMIT_TOOL_NAME, submission::decode_submission};

/// Decoding is not acceptance. Invalid input is already answered by paired
/// repair feedback; a valid value still requires explicit revision or acceptance.
#[derive(Debug)]
pub enum Submission<T> {
    Ready(T),
    Repaired { feedback: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SubmissionError {
    #[error(transparent)]
    Conversation(#[from] ConversationError),
    #[error("action is not a pending submission")]
    NotSubmission,
    #[error("submission must pass typed decoding and schema validation before acceptance")]
    NotValidated,
    #[error("submission schema is invalid: {0}")]
    Schema(String),
    #[error("no model response is available to repair")]
    NoResponse,
    #[error("the response contains a submission; resolve its handle instead")]
    SubmissionPresent,
}

/// The same schema offered to a model is checked when decoding its submission.
pub fn submission_tool<T: JsonSchema>() -> ToolDefinition {
    ToolDefinition {
        name: SUBMIT_TOOL_NAME.into(),
        description: "Submit the final structured result. Call this once you are confident.".into(),
        parameters: schemars::schema_for!(T).to_value(),
    }
}

impl Conversation {
    /// Decode one proposal, retaining its untouched original arguments. The
    /// existing one-level wrapper and JSON-text collection repairs are supported.
    /// Successful serde decoding must also satisfy T's JSON Schema. Schema setup
    /// errors leave the action pending and never become model repair feedback.
    pub fn submission<T: DeserializeOwned + JsonSchema>(
        &mut self,
        handle: &PendingAction,
    ) -> Result<Submission<T>, SubmissionError> {
        self.require_submission(handle)?;
        let definition = submission_tool::<T>();
        let validator = jsonschema::validator_for(&definition.parameters)
            .map_err(|error| SubmissionError::Schema(error.to_string()))?;
        let args = &self.action(handle)?.call.function.arguments;
        match decode_submission(args, Some(&validator), true) {
            Ok(value) => {
                self.mark_submission_validated(handle)?;
                Ok(Submission::Ready(value))
            }
            Err(error) => {
                let feedback = format!(
                    "Your `submit` call did not match the required schema and was NOT accepted: \
                     {error}. Call `submit` again with every required field present."
                );
                self.revise_submission(handle, feedback.clone())?;
                Ok(Submission::Repaired { feedback })
            }
        }
    }

    /// Answer this submission with application revision feedback. Sibling calls
    /// remain pending; the complete result batch is emitted only after all resolve.
    pub fn revise_submission(
        &mut self,
        handle: &PendingAction,
        feedback: impl Into<String>,
    ) -> Result<(), SubmissionError> {
        self.require_submission(handle)?;
        self.resolve(
            handle,
            ActionResolution::Rejected(ToolOutput::text(feedback)),
        )?;
        Ok(())
    }

    /// Explicitly accept caller-owned A after this exact proposal passed decoding.
    /// Every other proposal must already be resolved or rejected. No sibling is
    /// silently discarded, and A is not written over the original submission.
    pub fn accept_submission<A>(
        &mut self,
        handle: &PendingAction,
        accepted: A,
    ) -> Result<A, SubmissionError> {
        self.require_submission(handle)?;
        if !self.action(handle)?.submission_validated {
            return Err(SubmissionError::NotValidated);
        }
        if self.pending_count() != 1 {
            return Err(ConversationError::PendingActions(self.pending_count() - 1).into());
        }
        self.resolve(
            handle,
            ActionResolution::Result(ToolOutput::text("Submission accepted.")),
        )?;
        self.finish()?;
        Ok(accepted)
    }

    /// Request a submission after prose or a completed tool-only response. This
    /// appends ordinary user feedback, never an invented tool call/result. Pending
    /// tools must first be resolved; responses containing submit use their handles.
    pub fn repair_missing_submission(&mut self) -> Result<(), SubmissionError> {
        self.request_messages()?;
        let response = self.responses().last().ok_or(SubmissionError::NoResponse)?;
        if response
            .actions()
            .iter()
            .any(|action| action.kind() == ActionKind::Submission)
        {
            return Err(SubmissionError::SubmissionPresent);
        }
        self.push_user(vec![UserContent::text(format!(
            "You replied without a recorded submission. Call the `{SUBMIT_TOOL_NAME}` tool with your answer."
        ))])?;
        Ok(())
    }

    fn require_submission(&self, handle: &PendingAction) -> Result<(), SubmissionError> {
        let action = self.action(handle)?;
        if action.kind != ActionKind::Submission {
            return Err(SubmissionError::NotSubmission);
        }
        if action.status == ActionStatus::Started {
            return Err(ConversationError::AlreadyStarted.into());
        }
        Ok(())
    }
}
