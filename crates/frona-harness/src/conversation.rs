//! In-memory conversation and action state shared by manual and automatic drivers.

use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolResult, UserContent},
    request::ToolDefinition,
};
use uuid::Uuid;

use super::{ModelOutput, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Tool,
    Submission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    Proposed,
    Started,
    Resolved,
    Rejected,
}

/// A paired protocol result. Rejection records feedback without claiming an
/// invocation occurred. Neither variant accepts an application refinement result.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionResolution {
    Result(ToolOutput),
    /// An invoked tool failed ordinarily; the paired output is model feedback.
    Failed(ToolOutput),
    Rejected(ToolOutput),
}

impl ActionResolution {
    pub fn output(&self) -> &ToolOutput {
        match self {
            Self::Result(output) | Self::Failed(output) | Self::Rejected(output) => output,
        }
    }
}

/// An owned, non-Clone capability identifying one proposed action. It does not
/// borrow the conversation and contains no editable provider data. Resolution
/// still validates its identity and state, even if a caller reuses a reference.
///
/// ```compile_fail
/// use frona_harness::PendingAction;
/// fn duplicate(action: PendingAction) {
///     let _duplicate = action.clone();
/// }
/// ```
#[derive(Debug)]
pub struct PendingAction {
    conversation: Uuid,
    response: usize,
    slot: usize,
}

/// Every proposal in one recorded response, in original content order.
#[derive(Debug)]
pub struct ResponseActions {
    pub response_index: usize,
    pub actions: Vec<PendingAction>,
}

/// Read-only action information obtained only after validating a handle.
#[derive(Debug)]
pub struct PendingActionView<'a> {
    pub call: &'a ToolCall,
    pub kind: ActionKind,
    pub status: ActionStatus,
    pub offered_tools: &'a [ToolDefinition],
    /// This exact proposal has passed typed submission decoding and schema checks.
    pub submission_validated: bool,
}

#[derive(Debug)]
pub struct ActionRecord {
    call: ToolCall,
    kind: ActionKind,
    status: ActionStatus,
    resolution: Option<ActionResolution>,
    submission_validated: bool,
}

impl ActionRecord {
    pub fn call(&self) -> &ToolCall {
        &self.call
    }
    pub fn kind(&self) -> ActionKind {
        self.kind
    }
    pub fn status(&self) -> ActionStatus {
        self.status
    }
    pub fn resolution(&self) -> Option<&ActionResolution> {
        self.resolution.as_ref()
    }
    pub fn submission_validated(&self) -> bool {
        self.submission_validated
    }
}

/// Raw provider output and the exact tools offered for that request. The output
/// is never normalized or rewritten when actions are resolved.
#[derive(Debug)]
pub struct ResponseRecord {
    output: ModelOutput,
    offered_tools: Vec<ToolDefinition>,
    actions: Vec<ActionRecord>,
}

impl ResponseRecord {
    pub fn output(&self) -> &ModelOutput {
        &self.output
    }
    pub fn offered_tools(&self) -> &[ToolDefinition] {
        &self.offered_tools
    }
    pub fn actions(&self) -> &[ActionRecord] {
        &self.actions
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationStatus {
    Active,
    Finished,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    #[error("conversation is terminal")]
    Terminal,
    #[error("{0} actions still require a result")]
    PendingActions(usize),
    #[error("tool results must resolve a recorded action")]
    UnpairedToolResult,
    #[error("action belongs to another conversation")]
    ForeignAction,
    #[error("action belongs to an earlier or unknown response")]
    StaleAction,
    #[error("action has already been started")]
    AlreadyStarted,
    #[error("action has already been resolved")]
    AlreadyResolved,
}

/// Portable protocol state; it owns no model, tools, identities, or persistence.
/// Recording and resolution execute nothing. The next model request is available
/// only after every proposal has a paired result. Stopping never implies success.
#[derive(Debug)]
pub struct Conversation {
    id: Uuid,
    messages: Vec<Message>,
    responses: Vec<ResponseRecord>,
    pending: usize,
    status: ConversationStatus,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            id: Uuid::now_v7(),
            messages: Vec::new(),
            responses: Vec::new(),
            pending: 0,
            status: ConversationStatus::Active,
        }
    }
}

impl Conversation {
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user(input)],
            ..Self::default()
        }
    }

    /// Original history, including an outstanding assistant proposal. Use
    /// `request_messages` when preparing another model call.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
    pub fn responses(&self) -> &[ResponseRecord] {
        &self.responses
    }
    pub fn status(&self) -> ConversationStatus {
        self.status
    }
    pub fn pending_count(&self) -> usize {
        self.pending
    }

    pub fn request_messages(&self) -> Result<&[Message], ConversationError> {
        self.require_ready()?;
        Ok(&self.messages)
    }

    /// Add caller input or conversational feedback, without inventing call IDs.
    pub fn push_user(&mut self, content: Vec<UserContent>) -> Result<(), ConversationError> {
        self.require_ready()?;
        if content
            .iter()
            .any(|item| matches!(item, UserContent::ToolResult(_)))
        {
            return Err(ConversationError::UnpairedToolResult);
        }
        self.messages.push(Message::User { content });
        Ok(())
    }

    /// Record all provider content before exposing proposals. Native structured
    /// output without a ToolCall remains in the raw record; no call is invented.
    /// Duplicate provider IDs remain distinct slots without rewriting the IDs.
    pub fn record_response(
        &mut self,
        output: ModelOutput,
        offered_tools: Vec<ToolDefinition>,
    ) -> Result<ResponseActions, ConversationError> {
        self.require_ready()?;
        let response_index = self.responses.len();
        let actions: Vec<_> = output
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some(ActionRecord {
                    kind: if call.function.name == crate::inference::provider::SUBMIT_TOOL_NAME {
                        ActionKind::Submission
                    } else {
                        ActionKind::Tool
                    },
                    call: call.clone(),
                    status: ActionStatus::Proposed,
                    resolution: None,
                    submission_validated: false,
                }),
                _ => None,
            })
            .collect();
        let handles = (0..actions.len())
            .map(|slot| PendingAction {
                conversation: self.id,
                response: response_index,
                slot,
            })
            .collect();
        self.pending = actions.len();
        self.messages.push(Message::Assistant {
            id: None,
            content: output.content.clone(),
        });
        self.responses.push(ResponseRecord {
            output,
            offered_tools,
            actions,
        });
        Ok(ResponseActions {
            response_index,
            actions: handles,
        })
    }

    pub fn action(
        &self,
        handle: &PendingAction,
    ) -> Result<PendingActionView<'_>, ConversationError> {
        let action = self.validate(handle)?;
        Ok(PendingActionView {
            call: &action.call,
            kind: action.kind,
            status: action.status,
            offered_tools: &self.responses[handle.response].offered_tools,
            submission_validated: action.submission_validated,
        })
    }

    pub(super) fn mark_submission_validated(
        &mut self,
        handle: &PendingAction,
    ) -> Result<(), ConversationError> {
        self.validate(handle)?;
        self.responses[handle.response].actions[handle.slot].submission_validated = true;
        Ok(())
    }

    /// Reserve a proposal before an external effect. A second start is rejected.
    /// This state transition itself performs no effect or budget admission.
    pub fn start(&mut self, handle: &PendingAction) -> Result<(), ConversationError> {
        if self.validate(handle)?.status == ActionStatus::Started {
            return Err(ConversationError::AlreadyStarted);
        }
        self.responses[handle.response].actions[handle.slot].status = ActionStatus::Started;
        Ok(())
    }

    /// Resolve one proposal. Results may arrive out of order; the replay message
    /// is appended once, in proposal order, after the entire batch is resolved.
    /// A proposed action can be resolved without `start` for rejection/repair or
    /// caller-supplied results. Drivers own invocation and acceptance policy.
    pub fn resolve(
        &mut self,
        handle: &PendingAction,
        resolution: ActionResolution,
    ) -> Result<(), ConversationError> {
        if self.validate(handle)?.status == ActionStatus::Started
            && matches!(resolution, ActionResolution::Rejected(_))
        {
            return Err(ConversationError::AlreadyStarted);
        }
        let response = &mut self.responses[handle.response];
        let action = &mut response.actions[handle.slot];
        action.status = match resolution {
            ActionResolution::Result(_) | ActionResolution::Failed(_) => ActionStatus::Resolved,
            ActionResolution::Rejected(_) => ActionStatus::Rejected,
        };
        action.resolution = Some(resolution);
        self.pending -= 1;
        if self.pending == 0 {
            let content = response
                .actions
                .iter()
                .map(|action| {
                    UserContent::ToolResult(ToolResult {
                        call: action.call.id.clone(),
                        provider: action.call.provider.clone(),
                        name: action.call.function.name.clone(),
                        content: action
                            .resolution
                            .as_ref()
                            .expect("all actions resolved")
                            .output()
                            .content
                            .clone(),
                    })
                })
                .collect();
            self.messages.push(Message::User { content });
        }
        Ok(())
    }

    /// End a protocol-complete exchange. This is not inspector acceptance and
    /// does not create an accepted refinement value.
    pub fn finish(&mut self) -> Result<(), ConversationError> {
        self.require_ready()?;
        self.status = ConversationStatus::Finished;
        Ok(())
    }

    /// End unfinished work while retaining unresolved proposals and raw history.
    pub fn stop(&mut self) {
        if self.status == ConversationStatus::Active {
            self.status = ConversationStatus::Stopped;
        }
    }

    fn require_ready(&self) -> Result<(), ConversationError> {
        if self.status != ConversationStatus::Active {
            return Err(ConversationError::Terminal);
        }
        if self.pending != 0 {
            return Err(ConversationError::PendingActions(self.pending));
        }
        Ok(())
    }

    fn validate(&self, handle: &PendingAction) -> Result<&ActionRecord, ConversationError> {
        if handle.conversation != self.id {
            return Err(ConversationError::ForeignAction);
        }
        if self.status != ConversationStatus::Active {
            return Err(ConversationError::Terminal);
        }
        if handle.response.checked_add(1) != Some(self.responses.len()) {
            return Err(ConversationError::StaleAction);
        }
        let action = self.responses[handle.response]
            .actions
            .get(handle.slot)
            .ok_or(ConversationError::StaleAction)?;
        if action.resolution.is_some() {
            return Err(ConversationError::AlreadyResolved);
        }
        Ok(action)
    }
}
