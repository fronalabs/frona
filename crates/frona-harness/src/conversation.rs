//! In-memory conversation and action state shared by manual and automatic drivers.

mod replay;
pub use replay::{HistoryReplay, replay_history};
mod checkpoint;
pub use checkpoint::{CheckpointLoadError, CheckpointRef, LoadedCheckpoint};

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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ActionResolution {
    Result(#[serde(with = "super::history::tool_output")] ToolOutput),
    /// An invoked tool failed ordinarily; the paired output is model feedback.
    Failed(#[serde(with = "super::history::tool_output")] ToolOutput),
    Rejected(#[serde(with = "super::history::tool_output")] ToolOutput),
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
    invocation_ordinal: u64,
}

impl ActionRecord {
    /// Number of durably recorded invocation starts, including interrupted attempts.
    pub fn invocation_ordinal(&self) -> u64 {
        self.invocation_ordinal
    }
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
    #[error("loaded storage must be prepared by explicit resume before mutation")]
    StorageNotReady,
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
    #[error("action has not been started")]
    NotStarted,
    #[error("action invocation ordinal overflow")]
    InvocationOverflow,
    #[error("submission actions cannot be started as tool invocations")]
    SubmissionStart,
    #[error("action has already been resolved")]
    AlreadyResolved,
}

/// Portable protocol state with optional reliable history storage. It owns no
/// model, tools, or runtime identities.
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
    #[cfg(feature = "jsonl")]
    pending_jsonl: Option<super::jsonl::JsonlContinuation>,
    instructions: String,
    mode: HistoryMode,
    compacted: Option<super::CompactedView>,
}

/// Borrowed provenance for one projected model-facing conversation suffix.
/// Constructed only by a ready conversation; it carries no editable identities.
/// Keep this value alive through the model call and send the same projection.
pub struct ConversationInput<'a> {
    conversation: &'a Conversation,
    input: RequestInput,
}

impl ConversationInput<'_> {
    pub fn execution<'a>(
        &'a self,
        execution: super::ExecutionControl<'a>,
    ) -> Result<super::ExecutionControl<'a>, ConversationError> {
        if let (Some(expected), Some(actual)) =
            (self.conversation.report.as_ref(), execution.report_handle())
            && !expected.same_scope(actual)
        {
            return Err(ConversationError::InvalidView(
                "input belongs to another report scope".into(),
            ));
        }
        Ok(execution.with_input(&self.input))
    }
}

impl Default for Conversation {
    fn default() -> Self {
        Self::empty_with_id(Uuid::now_v7())
    }
}

impl Conversation {
    /// Start a fresh live suffix tracked by the supplied execution scope.
    /// Persisted host prefixes are not fabricated into live action history.
    pub fn from_execution(execution: super::ExecutionControl<'_>) -> Self {
        match execution.report_handle() {
            Some(report) => Self::default().with_report(report.clone()),
            None => Self::default(),
        }
    }

    /// Correlate a position-preserving native projection of the selected view.
    /// Changed messages are never counted as literal feedback delivery. Summary
    /// entries have no original position; retained entries keep their original
    /// positions even after compaction. A host prefix must not be included here.
    pub fn projected_input(
        &self,
        projected: &[Message],
    ) -> Result<ConversationInput<'_>, ConversationError> {
        self.require_ready()?;
        let view = self
            .model_view()
            .map_err(|error| ConversationError::InvalidView(error.to_string()))?;
        let selected = view.messages();
        if selected.len() != projected.len()
            || selected.iter().zip(projected).any(|(original, projected)| {
                std::mem::discriminant(original) != std::mem::discriminant(projected)
            })
        {
            return Err(ConversationError::InvalidView(
                "projection must preserve message positions and roles".into(),
            ));
        }
        let messages = view
            .entries()
            .iter()
            .zip(selected.iter().zip(projected))
            .filter_map(|(entry, (selected, projected))| match entry {
                super::HistoryViewEntry::Original(position) if selected == projected => {
                    Some(*position)
                }
                _ => None,
            })
            .collect();
        Ok(ConversationInput {
            conversation: self,
            input: self.request_input(messages),
        })
    }

    fn empty_with_id(id: Uuid) -> Self {
        Self {
            id,
            messages: Vec::new(),
            responses: Vec::new(),
            pending: 0,
            status: ConversationStatus::Active,
            report: None,
            progress: ProgressPublisher::default(),
            history: History::default(),
            #[cfg(feature = "jsonl")]
            pending_jsonl: None,
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

    /// Attach a newly created JSONL writer before the first recorded transition.
    /// Configuration remains mutable until that transition commits `Opened`.
    #[cfg(feature = "jsonl")]
    pub fn attach_jsonl(&mut self, writer: super::JsonlWriter) -> Result<(), super::JsonlError> {
        if self.pending_jsonl.is_some() || !self.history.can_attach_new() {
            return Err(super::JsonlError::AlreadyAttached);
        }
        self.history.attach(Box::new(writer));
        Ok(())
    }

    #[cfg(feature = "jsonl")]
    pub(super) fn attach_restored_jsonl(&mut self, writer: super::JsonlWriter) {
        self.history.attach_restored(Box::new(writer));
    }

    #[cfg(feature = "jsonl")]
    pub(super) fn with_pending_jsonl(mut self, source: super::jsonl::JsonlContinuation) -> Self {
        self.pending_jsonl = Some(source);
        self
    }

    #[cfg(feature = "jsonl")]
    pub(super) fn prepare_jsonl(&mut self) -> Result<(), ConversationError> {
        if let Some(source) = &self.pending_jsonl {
            match source.prepare() {
                Ok(writer) => {
                    self.attach_restored_jsonl(writer);
                    self.pending_jsonl = None;
                }
                Err(error) => {
                    let failure = self
                        .history
                        .resume_failed(error.resume_phase(), error.to_string());
                    self.mark_stopped();
                    return Err(failure.into());
                }
            }
        }
        Ok(())
    }

    pub(super) fn require_storage_ready(&self) -> Result<(), ConversationError> {
        #[cfg(feature = "jsonl")]
        if self.pending_jsonl.is_some() {
            return Err(ConversationError::StorageNotReady);
        }
        Ok(())
    }

    pub(super) fn saved_configuration(&self) -> Option<(&str, &HistoryMode)> {
        match self.history().first().map(|record| &record.event) {
            Some(HistoryEvent::Opened {
                instructions, mode, ..
            }) => Some((instructions, mode)),
            _ => None,
        }
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
        self.require_storage_ready()?;
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
    /// provenance stays attached. This does not adopt its pending transition or
    /// reactivate a completed history whose later checkpoint write failed.
    pub fn into_memory(mut self) -> Result<Self, ConversationError> {
        let detached = self.history.detach();
        #[cfg(feature = "jsonl")]
        let detached = self.pending_jsonl.take().is_some() || detached;
        if !detached {
            return Err(ConversationError::NotStorageStopped);
        }
        if self.status != ConversationStatus::Finished {
            self.status = ConversationStatus::Active;
        }
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
                | HistoryEvent::Checkpoint { .. }
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
    /// A failed checkpoint after completion may be discarded without reopening
    /// execution; a later explicit checkpoint can save new application state.
    pub fn discard_pending_write(&mut self) -> Result<(), ConversationError> {
        let terminal_checkpoint = self.status == ConversationStatus::Finished
            && self
                .history
                .candidate()
                .is_some_and(|r| matches!(r.event, HistoryEvent::Checkpoint { .. }));
        if self.history.failure().is_some()
            || (self.status != ConversationStatus::Active && !terminal_checkpoint)
        {
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
        self.seed_report(&report);
        self.report = Some(report);
        self
    }

    fn seed_report(&self, report: &ReportHandle) {
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
                    invocation_ordinal: 0,
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
                name: action.call.function.name.to_string(),
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
        self.messages.push(Message::Assistant(
            rig_core::completion::message::AssistantMessage::new(output.content.clone())
                .with_origin(output.origin.clone())
                .with_stop(output.stop.clone()),
        ));
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
        self.start_invocation(handle, false)
    }

    pub(super) fn start_invocation(
        &mut self,
        handle: &PendingAction,
        recovery: bool,
    ) -> Result<(), ConversationError> {
        let ordinal = self.next_invocation(handle, recovery)?;
        self.ensure_history()?;
        self.commit(
            HistoryEvent::ToolStarted {
                action: self.action_position(handle)?,
                ordinal,
            },
            EffectState::Prevented,
        )?;
        self.responses[handle.response].actions[handle.slot].status = ActionStatus::Started;
        self.responses[handle.response].actions[handle.slot].invocation_ordinal = ordinal;
        if let Some(report) = &self.report {
            report.started(self.action_position(handle)?);
        }
        self.emit_action(handle);
        Ok(())
    }

    pub(super) fn next_invocation(
        &self,
        handle: &PendingAction,
        recovery: bool,
    ) -> Result<u64, ConversationError> {
        let action = self.validate(handle)?;
        if action.kind == ActionKind::Submission {
            return Err(ConversationError::SubmissionStart);
        }
        match (recovery, action.status) {
            (false, ActionStatus::Started) => return Err(ConversationError::AlreadyStarted),
            (true, ActionStatus::Proposed) => return Err(ConversationError::NotStarted),
            _ => {}
        }
        action
            .invocation_ordinal
            .checked_add(1)
            .ok_or(ConversationError::InvocationOverflow)
    }

    pub(super) fn activate_recovery(&mut self, report: Option<&ReportHandle>) {
        // A live incomplete Conversation may still hold its previous recorder.
        // Only a newly supplied, nonfinalized scope can receive recovery evidence.
        let same_scope = self
            .report
            .as_ref()
            .zip(report)
            .is_some_and(|(old, new)| old.same_scope(new));
        if !same_scope {
            self.report = None;
            self.progress = ProgressPublisher::default();
            if let Some(report) = report {
                self.seed_report(report);
                self.report = Some(report.clone());
            }
        }
        self.status = ConversationStatus::Active;
        for response in &mut self.responses {
            for action in &mut response.actions {
                action.submission_validated = false;
            }
        }
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
                        name: action.call.function.name.clone(),
                        is_error: !matches!(action.resolution, Some(ActionResolution::Result(_))),
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
                name: action.call.function.name.to_string(),
                kind: action.kind,
                status: action.status,
            }
        });
    }

    /// Extract the same final answer for live text completion and pure replay.
    /// A response containing tool proposals is not final, even after resolution.
    pub(super) fn final_text(&self) -> Option<String> {
        let response = self.responses.last()?;
        if !response.actions.is_empty() {
            return None;
        }
        let text: String = response
            .output
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        (!text.is_empty()).then_some(text)
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

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod projection_tests;
