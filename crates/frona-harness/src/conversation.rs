//! In-memory conversation and action state shared by manual and automatic drivers.

use super::history::History;
#[cfg(test)]
use super::history::HistoryWriter;
use super::{
    EffectState, HistoryCompletion, HistoryEvent, HistoryFailure, HistoryMode, HistoryRecord,
    SubmissionCompletion,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolResult, UserContent},
    request::ToolDefinition,
};
use std::sync::Arc;
use uuid::Uuid;

use super::{
    ActionPosition, ModelOutput, Progress, ReportHandle, RequestInput, ToolOutput,
    progress::ProgressPublisher,
};

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
    message_position: usize,
    output: ModelOutput,
    origin: Option<String>,
    offered_tools: Vec<ToolDefinition>,
    actions: Vec<ActionRecord>,
}

impl ResponseRecord {
    /// Original assistant-message position, unaffected by compaction.
    pub fn message_position(&self) -> usize {
        self.message_position
    }
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }
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
    #[error("invalid selected model view: {0}")]
    InvalidView(String),
    #[error(transparent)]
    Storage(#[from] Arc<HistoryFailure>),
    #[error("conversation is not stopped by a history write failure")]
    NotStorageStopped,
    #[error("choose whether to adopt the pending history write before continuing")]
    PendingWrite,
    #[error("this pending write cannot be adopted as a completed transition")]
    CannotAdoptWrite,
    #[error("a known response or result must be adopted, not discarded")]
    CannotDiscardWrite,
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
    #[error("submission actions cannot be started as tool invocations")]
    SubmissionStart,
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
    report: Option<ReportHandle>,
    progress: ProgressPublisher,
    history: History,
    instructions: String,
    mode: HistoryMode,
    compacted: Option<super::CompactedView>,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            id: Uuid::now_v7(),
            messages: Vec::new(),
            responses: Vec::new(),
            pending: 0,
            status: ConversationStatus::Active,
            report: None,
            progress: ProgressPublisher::default(),
            history: History::default(),
            instructions: String::new(),
            mode: HistoryMode::Text,
            compacted: None,
        }
    }
}

impl Conversation {
    /// The selected model-facing view, separate from immutable original messages.
    pub fn model_view(&self) -> Result<super::HistoryView<'_>, super::CompactionError> {
        match &self.compacted {
            Some(view) => super::HistoryView::from_compacted(&self.messages, view),
            None => Ok(super::HistoryView::new(&self.messages)),
        }
    }

    pub fn compacted_view(&self) -> Option<&super::CompactedView> {
        self.compacted.as_ref()
    }

    pub(super) fn commit_compaction(
        &mut self,
        view: super::CompactedView,
        summarizer: String,
        output: ModelOutput,
    ) -> Result<(), ConversationError> {
        self.require_ready()?;
        self.ensure_history()?;
        self.commit(
            HistoryEvent::Compacted {
                view: view.clone(),
                summarizer,
                output,
            },
            EffectState::Returned,
        )?;
        self.compacted = Some(view);
        Ok(())
    }
    pub fn history(&self) -> &[HistoryRecord] {
        self.history.records()
    }
    pub fn history_failure(&self) -> Option<&HistoryFailure> {
        self.history.failure().map(Arc::as_ref)
    }
    pub fn pending_write(&self) -> Option<&HistoryRecord> {
        self.history.candidate()
    }
    pub fn detached_failures(&self) -> &[Arc<HistoryFailure>] {
        self.history.detached_failures()
    }

    #[cfg(test)]
    pub(crate) fn attach_history(&mut self, writer: Box<dyn HistoryWriter>) {
        self.history.attach(writer);
    }

    pub(super) fn configure_history(
        &mut self,
        instructions: &str,
        submission: Option<&ToolDefinition>,
    ) {
        if self.history.records().is_empty() && self.history.failure().is_none() {
            self.instructions = instructions.into();
            self.mode = submission
                .map(|s| HistoryMode::Structured(s.parameters.clone()))
                .unwrap_or(HistoryMode::Text);
        }
    }

    pub(super) fn ensure_history(&mut self) -> Result<(), ConversationError> {
        if let Some(failure) = self.history.failure() {
            return Err(failure.clone().into());
        }
        if self.history.candidate().is_some() {
            return Err(ConversationError::PendingWrite);
        }
        if self.history.records().is_empty() {
            let initial = match self.messages.first() {
                Some(Message::User { content }) => Some(content.clone()),
                _ => None,
            };
            self.commit(
                HistoryEvent::Opened {
                    initial,
                    instructions: self.instructions.clone(),
                    mode: self.mode.clone(),
                },
                EffectState::Prevented,
            )?;
        }
        Ok(())
    }

    fn commit(
        &mut self,
        event: HistoryEvent,
        effect: EffectState,
    ) -> Result<(), ConversationError> {
        if let Err(failure) = self.history.commit(self.id, event, effect) {
            self.mark_stopped();
            return Err(failure.into());
        }
        Ok(())
    }

    /// Explicitly abandon an uncertain writer. No file is changed, and failure
    /// provenance stays attached. This does not adopt its pending transition.
    pub fn into_memory(mut self) -> Result<Self, ConversationError> {
        if !self.history.detach() {
            return Err(ConversationError::NotStorageStopped);
        }
        self.status = ConversationStatus::Active;
        self.report = None;
        self.progress = ProgressPublisher::default();
        for response in &mut self.responses {
            for action in &mut response.actions {
                action.submission_validated = false;
            }
        }
        Ok(self)
    }

    /// Adopt a known response/result/feedback after explicit memory detachment.
    /// Completion candidates are never adopted: validate and complete anew.
    pub fn adopt_pending_write(&mut self) -> Result<(), ConversationError> {
        if self.history.failure().is_some() || self.status != ConversationStatus::Active {
            return Err(ConversationError::Terminal);
        }
        let Some(candidate) = self.history.candidate() else {
            return Ok(());
        };
        if matches!(
            candidate.event,
            HistoryEvent::SubmissionCompleted { .. }
                | HistoryEvent::Finished { .. }
                | HistoryEvent::ToolStarted { .. }
                | HistoryEvent::Stopped { .. }
                | HistoryEvent::Compacted { .. }
        ) {
            return Err(ConversationError::CannotAdoptWrite);
        }
        let candidate = self.history.take_candidate().expect("checked candidate");
        match candidate.event {
            HistoryEvent::Opened {
                initial,
                instructions,
                mode,
            } => {
                self.commit(
                    HistoryEvent::Opened {
                        initial,
                        instructions,
                        mode,
                    },
                    EffectState::NoEffect,
                )?;
            }
            HistoryEvent::Response {
                origin,
                output,
                offered_tools,
                ..
            } => {
                self.record_response_from(output, offered_tools, origin)?;
            }
            HistoryEvent::User { content, .. } => self.push_user(content)?,
            HistoryEvent::ActionResolved { action, resolution } => {
                self.resolve(
                    &PendingAction {
                        conversation: action.conversation,
                        response: action.response,
                        slot: action.action,
                    },
                    resolution,
                )?;
            }
            _ => unreachable!("non-adoptable events checked above"),
        }
        Ok(())
    }

    /// Discard an unacknowledged intent. A returned model/tool result must be
    /// adopted explicitly instead, so forgetting it cannot silently repeat work.
    pub fn discard_pending_write(&mut self) -> Result<(), ConversationError> {
        if self.history.failure().is_some() || self.status != ConversationStatus::Active {
            return Err(ConversationError::Terminal);
        }
        if self.history.candidate().is_some_and(|r| {
            matches!(
                r.event,
                HistoryEvent::Response { .. } | HistoryEvent::ActionResolved { .. }
            )
        }) {
            return Err(ConversationError::CannotDiscardWrite);
        }
        self.history.take_candidate();
        Ok(())
    }

    pub(super) fn stop_with_reason(
        &mut self,
        category: super::FailureKind,
        message: String,
    ) -> Result<(), ConversationError> {
        if let Some(failure) = self.history.failure() {
            return Err(failure.clone().into());
        }
        if self.status == ConversationStatus::Finished {
            return Ok(());
        }
        // A stopped state can arise at the lower dispatch boundary before its
        // diagnostic is recorded. The first session failure writes it once.
        if !matches!(
            self.history.records().last().map(|r| &r.event),
            Some(HistoryEvent::Stopped { .. })
        ) {
            self.ensure_history()?;
            let effect = if self.responses.last().is_some_and(|response| {
                response
                    .actions
                    .iter()
                    .any(|a| a.status == ActionStatus::Started)
            }) {
                EffectState::InProgress
            } else {
                EffectState::NoEffect
            };
            self.commit(HistoryEvent::Stopped { category, message }, effect)?;
        }
        self.mark_stopped();
        Ok(())
    }

    pub(super) fn with_progress(mut self, progress: ProgressPublisher) -> Self {
        self.progress = progress;
        self
    }

    pub(super) fn with_report(mut self, report: ReportHandle) -> Self {
        // Restored unresolved proposals belong to the new execution's action
        // scope, but no old invocation, usage or delivery is fabricated.
        for (response_index, response) in self.responses.iter().enumerate() {
            for (slot, action) in response.actions.iter().enumerate() {
                if action.resolution.is_none() {
                    report.proposed(
                        ActionPosition {
                            conversation: self.id,
                            response: response_index,
                            action: slot,
                        },
                        &action.call,
                        action.kind,
                    );
                }
            }
        }
        self.report = Some(report);
        self
    }

    pub(super) fn request_input(&self, messages: Vec<usize>) -> RequestInput {
        RequestInput {
            conversation: self.id,
            messages,
        }
    }

    pub(super) fn action_position(
        &self,
        handle: &PendingAction,
    ) -> Result<ActionPosition, ConversationError> {
        self.validate(handle)?;
        Ok(ActionPosition {
            conversation: self.id,
            response: handle.response,
            action: handle.slot,
        })
    }

    pub fn new(input: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user(input)],
            ..Self::default()
        }
    }

    /// Original history, including an outstanding assistant proposal. Use
    /// `model_view` for the selected request content.
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

    /// Reissue handles for the latest unresolved proposals. This recovers a
    /// dropped/consumed handle, not execution rights: every use still validates
    /// current state, and started actions cannot execute a second time.
    pub fn pending_actions(&self) -> Result<ResponseActions, ConversationError> {
        if self.status != ConversationStatus::Active {
            return Err(ConversationError::Terminal);
        }
        let response_index = self.responses.len().saturating_sub(1);
        let actions = self
            .responses
            .last()
            .map(|response| {
                response
                    .actions
                    .iter()
                    .enumerate()
                    .filter(|(_, action)| action.resolution.is_none())
                    .map(|(slot, _)| PendingAction {
                        conversation: self.id,
                        response: response_index,
                        slot,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ResponseActions {
            response_index,
            actions,
        })
    }

    /// Ready-to-send messages from the selected view. Original history remains
    /// available through `messages`; compaction never rewrites those originals.
    pub fn request_messages(&self) -> Result<Vec<Message>, ConversationError> {
        self.require_ready()?;
        self.model_view()
            .map(|view| view.messages())
            .map_err(|error| ConversationError::InvalidView(error.to_string()))
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
        self.ensure_history()?;
        self.commit(
            HistoryEvent::User {
                message: self.messages.len(),
                content: content.clone(),
            },
            EffectState::NoEffect,
        )?;
        if let Some(report) = &self.report {
            report.message(self.id, self.messages.len(), None);
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
        self.record_response_from(output, offered_tools, None)
    }

    pub(super) fn record_response_from(
        &mut self,
        output: ModelOutput,
        offered_tools: Vec<ToolDefinition>,
        origin: Option<String>,
    ) -> Result<ResponseActions, ConversationError> {
        self.require_ready()?;
        self.ensure_history()?;
        self.commit(
            HistoryEvent::Response {
                response: self.responses.len(),
                message: self.messages.len(),
                origin: origin.clone(),
                offered_tools: offered_tools.clone(),
                output: output.clone(),
            },
            EffectState::Returned,
        )?;
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
        for (index, action) in actions.iter().enumerate() {
            self.progress.emit(|| Progress::Action {
                position: ActionPosition {
                    conversation: self.id,
                    response: response_index,
                    action: index,
                },
                name: action.call.function.name.clone(),
                kind: action.kind,
                status: action.status,
            });
        }
        if let Some(report) = &self.report {
            for (index, action) in actions.iter().enumerate() {
                report.proposed(
                    ActionPosition {
                        conversation: self.id,
                        response: response_index,
                        action: index,
                    },
                    &action.call,
                    action.kind,
                );
            }
        }
        self.messages.push(Message::Assistant {
            id: None,
            content: output.content.clone(),
        });
        self.responses.push(ResponseRecord {
            message_position: self.messages.len() - 1,
            output,
            origin,
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
        let action = self.validate(handle)?;
        if action.kind == ActionKind::Submission {
            return Err(ConversationError::SubmissionStart);
        }
        if action.status == ActionStatus::Started {
            return Err(ConversationError::AlreadyStarted);
        }
        self.ensure_history()?;
        self.commit(
            HistoryEvent::ToolStarted {
                action: self.action_position(handle)?,
                ordinal: 1,
            },
            EffectState::Prevented,
        )?;
        self.responses[handle.response].actions[handle.slot].status = ActionStatus::Started;
        if let Some(report) = &self.report {
            report.started(self.action_position(handle)?);
        }
        self.emit_action(handle);
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
        self.ensure_history()?;
        let effect = if self.validate(handle)?.status == ActionStatus::Started {
            EffectState::Returned
        } else {
            EffectState::NoEffect
        };
        self.commit(
            HistoryEvent::ActionResolved {
                action: self.action_position(handle)?,
                resolution: resolution.clone(),
            },
            effect,
        )?;
        self.apply_resolution(handle, resolution);
        Ok(())
    }

    fn apply_resolution(&mut self, handle: &PendingAction, resolution: ActionResolution) {
        let position = ActionPosition {
            conversation: self.id,
            response: handle.response,
            action: handle.slot,
        };
        if let Some(report) = &self.report {
            report.resolved(position, &resolution);
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
            if let Some(report) = &self.report {
                report.message(self.id, self.messages.len(), Some(handle.response));
            }
            self.messages.push(Message::User { content });
        }
        self.emit_action(handle);
    }

    pub(super) fn complete_submission(
        &mut self,
        handle: &PendingAction,
        kind: SubmissionCompletion,
    ) -> Result<(), ConversationError> {
        self.ensure_history()?;
        let acknowledgement = ToolOutput::text("Submission accepted.");
        self.commit(
            HistoryEvent::SubmissionCompleted {
                action: self.action_position(handle)?,
                acknowledgement: acknowledgement.clone(),
                kind,
            },
            EffectState::NoEffect,
        )?;
        self.apply_resolution(handle, ActionResolution::Result(acknowledgement));
        self.status = ConversationStatus::Finished;
        Ok(())
    }

    fn emit_action(&self, handle: &PendingAction) {
        self.progress.emit(|| {
            let action = &self.responses[handle.response].actions[handle.slot];
            Progress::Action {
                position: ActionPosition {
                    conversation: self.id,
                    response: handle.response,
                    action: handle.slot,
                },
                name: action.call.function.name.clone(),
                kind: action.kind,
                status: action.status,
            }
        });
    }

    /// End a protocol-complete exchange. This is not inspector acceptance and
    /// does not create an accepted refinement value.
    pub fn finish(&mut self) -> Result<(), ConversationError> {
        self.finish_with_kind(HistoryCompletion::Protocol)
    }

    pub(super) fn finish_with_kind(
        &mut self,
        kind: HistoryCompletion,
    ) -> Result<(), ConversationError> {
        self.require_ready()?;
        self.ensure_history()?;
        self.commit(HistoryEvent::Finished { kind }, EffectState::NoEffect)?;
        self.status = ConversationStatus::Finished;
        Ok(())
    }

    /// End unfinished work while retaining unresolved proposals and raw history.
    pub fn stop(&mut self) -> Result<(), ConversationError> {
        self.stop_with_reason(
            super::FailureKind::ApplicationStop,
            "conversation stopped".into(),
        )
    }

    pub(super) fn mark_stopped(&mut self) {
        if self.status == ConversationStatus::Active {
            self.status = ConversationStatus::Stopped;
            if let Some(report) = &self.report {
                report.stopped_actions(self.id);
            }
        }
    }

    pub(super) fn require_ready(&self) -> Result<(), ConversationError> {
        if self.history.candidate().is_some() && self.history.failure().is_none() {
            return Err(ConversationError::PendingWrite);
        }
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
