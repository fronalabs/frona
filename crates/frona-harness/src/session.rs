//! Configured runs and caller-driven execution over the shared SDK core.

use std::{future::IntoFuture, marker::PhantomData, num::NonZeroUsize, pin::Pin};

use rig_core::completion::{
    message::{ToolResultContent, UserContent},
    request::ToolDefinition,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::inference::{observation::Observations, provider::StreamToken};

use super::{
    ActionResolution, Budget, Conversation, ConversationError, DefaultBudget, ExecutionControl,
    ExecutionStop, Incomplete, Model, ModelError, ModelRequest, PendingAction, PendingActionView,
    Progress, ProgressReceiver, RefinementStage, ReportHandle, ResponseActions, RunOutcome,
    Submission, SubmissionError, Tool, ToolCallHook, ToolDecision, ToolDispatchError, ToolOutput,
    ToolRegistrationError, ToolRegistry, hook::CallbackHook, progress::ProgressOwner,
    report::RunRecorder, submission_tool,
};

/// Reusable model configuration. Each run starts fresh conversation and budget
/// state; explicitly supply a shared budget when work should share an allowance.
pub struct Harness<'a> {
    model: Model<'a>,
}

impl<'a> Harness<'a> {
    pub fn new(model: Model<'a>) -> Self {
        Self { model }
    }

    /// The run shares the prepared model without borrowing this harness. Its
    /// lifetime is constrained only by the model and dependencies added to it.
    pub fn run<'run>(&self, input: impl Into<String>) -> Run<'run, Text>
    where
        'a: 'run,
    {
        self.configured(Conversation::new(input), false)
    }

    /// Configure explicit continuation of owned memory or loaded JSONL history.
    /// No recovery, file mutation, or provider/tool work happens until execution.
    pub fn resume<'run>(&self, history: impl Into<Conversation>) -> Run<'run, Text>
    where
        'a: 'run,
    {
        self.configured(history.into(), true)
    }

    fn configured<'run>(&self, conversation: Conversation, resume_pending: bool) -> Run<'run, Text>
    where
        'a: 'run,
    {
        let instructions = conversation
            .saved_configuration()
            .map(|(instructions, _)| instructions.to_owned())
            .unwrap_or_default();
        let recorder = RunRecorder::new();
        let progress = ProgressOwner::default();
        let conversation = conversation
            .with_report(recorder.0.clone())
            .with_progress(progress.publisher());
        Run {
            core: SessionCore {
                model: self.model.clone(),
                conversation,
                recorder,
                progress,
                tools: ToolRegistry::default(),
                budget: Box::new(DefaultBudget::default()),
                cancellation: CancellationToken::new(),
                instructions,
                submission: None,
                setup_error: None,
                tool_hook: None,
                compaction: self.model.compaction_config(),
                compaction_configured: false,
                observer: None,
                exploration_turns: None,
                resume_pending,
                resume_response: false,
            },
            mode: PhantomData,
        }
    }
}

pub struct Text;
pub struct Structured<T>(PhantomData<fn() -> T>);

/// Configuration does not execute work. Registration errors are retained and
/// reported by the first execution operation, before any provider/tool effect.
/// Owned runs can outlive their harness, but borrowed dependencies cannot escape:
///
/// ```compile_fail
/// use frona_harness::{Harness, Model, ManualSession};
/// fn escape_model<'a>(model: Model<'a>) -> ManualSession<'static> {
///     Harness::new(model).run("input").manual()
/// }
/// ```
///
/// ```compile_fail
/// use frona_harness::{Harness, Model, ManualSession, DefaultBudget};
/// fn escape_budget(model: Model<'static>, budget: &DefaultBudget) -> ManualSession<'static> {
///     Harness::new(model).run("input").budget(budget).manual()
/// }
/// ```
///
/// ```compile_fail
/// use frona_harness::{Harness, Model, ManualSession, tool};
/// fn escape_tool(model: Model<'static>, value: &str) -> ManualSession<'static> {
///     Harness::new(model).run("input")
///         .tool(tool("borrowed", "Borrowed output", move |_: String| async move { Ok(value) }).unwrap())
///         .manual()
/// }
/// ```
pub struct Run<'a, Mode = Text> {
    pub(super) core: SessionCore<'a>,
    mode: PhantomData<fn() -> Mode>,
}

impl<'a, Mode> Run<'a, Mode> {
    /// Attach a freshly created file writer. Instructions and mode remain
    /// configurable until execution writes Opened. Errors use the normal outcome.
    #[cfg(feature = "jsonl")]
    pub fn jsonl(mut self, writer: super::JsonlWriter) -> Self {
        if self.core.setup_error.is_none() {
            self.core.setup_error = self
                .core
                .conversation
                .attach_jsonl(writer)
                .err()
                .map(ManualError::HistorySetup);
        }
        self
    }

    pub fn compaction(mut self, config: super::CompactionConfig) -> Self {
        self.core.compaction = config;
        self.core.compaction_configured = true;
        self
    }
    /// Disable summary calls while retaining context-overflow checks.
    pub fn disable_compaction(mut self) -> Self {
        self.core.compaction.enabled = false;
        self
    }

    /// Subscribe without starting work. Each receiver buffers 64 notifications.
    pub fn subscribe(&self) -> ProgressReceiver {
        self.subscribe_with_capacity(NonZeroUsize::new(64).unwrap())
    }

    /// An independent bounded buffer. Tokio rounds capacity up to a power of two.
    /// A lagging receiver observes `Progress::Missed`; it never blocks execution.
    pub fn subscribe_with_capacity(&self, capacity: NonZeroUsize) -> ProgressReceiver {
        self.core.progress.subscribe(capacity)
    }

    /// An independent read-only handle; this neither borrows nor starts the run.
    pub fn report(&self) -> ReportHandle {
        self.core.recorder.0.clone()
    }

    /// Optional telemetry for both ordinary calls and compaction. Observer errors
    /// are logged; they never become required-storage or execution failures.
    pub fn observer(
        mut self,
        observer: impl crate::inference::observation::InferenceObserver + 'a,
    ) -> Self {
        self.core.observer = Some(Box::new(observer));
        self
    }

    /// Offer application tools for at most this many response batches containing
    /// ordinary tool proposals. Submission-only responses and compaction do not
    /// consume this phase. This is not a budget: later submission requests still
    /// need ordinary shared admission. Restored runs configure a fresh phase.
    pub fn exploration_turns(mut self, turns: usize) -> Self {
        self.core.exploration_turns = Some(turns);
        self
    }

    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.core.instructions = instructions.into();
        self
    }
    pub fn tool(mut self, tool: impl Tool + 'a) -> Self {
        if self.core.setup_error.is_none() {
            self.core.setup_error = self.core.tools.register(tool).err().map(Into::into);
        }
        self
    }
    pub fn tools(mut self, tools: ToolRegistry<'a>) -> Self {
        if self.core.setup_error.is_none() {
            self.core.setup_error = self.core.tools.extend(tools).err().map(Into::into);
        }
        self
    }
    /// Pass a budget, its shared clone/Arc, or a reference to a borrowed policy.
    pub fn budget(mut self, budget: impl Budget + 'a) -> Self {
        self.core.budget = Box::new(budget);
        self
    }
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.core.cancellation = cancellation;
        self
    }
    /// Inspect an owned snapshot before each eligible ordinary tool invocation.
    /// Editing the snapshot cannot change canonical arguments. The callback may
    /// borrow application references; for mutable captures lending across await,
    /// use `tool_call_hook` with a ToolCallHook implementation.
    pub fn before_tool<F, Fut, E>(self, hook: F) -> Self
    where
        F: FnMut(rig_core::completion::message::ToolCall) -> Fut + Send + 'a,
        Fut: std::future::Future<Output = Result<ToolDecision, E>> + Send,
        E: std::fmt::Debug,
    {
        self.tool_call_hook(CallbackHook(hook))
    }

    /// Install a lending hook implementation. Like `before_tool`, this replaces
    /// any previously configured hook and is shared by manual execution.
    pub fn tool_call_hook(mut self, hook: impl ToolCallHook + 'a) -> Self {
        self.core.tool_hook = Some(Box::new(hook));
        self
    }
    pub fn structured<T: JsonSchema>(mut self) -> Run<'a, Structured<T>> {
        self.core.submission = Some(submission_tool::<T>());
        Run {
            core: self.core,
            mode: PhantomData,
        }
    }
    pub fn manual(self) -> ManualSession<'a, Mode> {
        ManualSession {
            core: self.core,
            mode: PhantomData,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManualError {
    #[error(transparent)]
    Resume(#[from] ResumeError),
    #[cfg(feature = "jsonl")]
    #[error(transparent)]
    HistorySetup(super::JsonlError),
    #[error(transparent)]
    Compaction(#[from] super::CompactionError),
    #[error(transparent)]
    Setup(#[from] ToolRegistrationError),
    #[error(transparent)]
    Conversation(ConversationError),
    #[error(transparent)]
    Submission(SubmissionError),
    #[error(transparent)]
    Dispatch(ToolDispatchError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Stop(#[from] ExecutionStop),
    #[error("cannot replay history to {model}: {reason}")]
    Incompatible { model: String, reason: &'static str },
    #[error("application abandoned refinement: {0}")]
    Abandoned(String),
    #[error("application inspector failed: {0}")]
    Inspector(String),
    #[error("application tool hook failed: {0}")]
    ToolHook(String),
    #[error("the current response has no final text")]
    NoText,
}

impl From<ConversationError> for ManualError {
    fn from(error: ConversationError) -> Self {
        match error {
            ConversationError::Storage(failure) => {
                Self::Stop(ExecutionStop::Storage(Box::new((*failure).clone())))
            }
            error => Self::Conversation(error),
        }
    }
}
impl From<SubmissionError> for ManualError {
    fn from(error: SubmissionError) -> Self {
        match error {
            SubmissionError::Conversation(error @ ConversationError::Storage(_)) => error.into(),
            error => Self::Submission(error),
        }
    }
}
impl From<ToolDispatchError> for ManualError {
    fn from(error: ToolDispatchError) -> Self {
        match error {
            ToolDispatchError::Conversation(error @ ConversationError::Storage(_)) => error.into(),
            ToolDispatchError::ToolHook(error) => Self::ToolHook(error),
            ToolDispatchError::Stop(stop @ ExecutionStop::Storage(_)) => Self::Stop(stop),
            error => Self::Dispatch(error),
        }
    }
}

/// Call selection changes the next request only, never history or allowance.
#[derive(Debug, Clone, Copy, Default)]
pub enum ToolSelection {
    #[default]
    All,
    None,
}

/// A manual driver owns the same configured core used by automatic drivers.
/// Model calls expose every action before any application tool can execute.
///
/// ```compile_fail,E0382
/// use frona_harness::{ManualSession, PendingAction};
/// async fn twice(session: &mut ManualSession<'_>, action: PendingAction) {
///     let _ = session.execute(action).await;
///     let _ = session.execute(action).await;
/// }
/// ```
pub struct ManualSession<'a, Mode = Text> {
    pub(super) core: SessionCore<'a>,
    mode: PhantomData<fn() -> Mode>,
}

impl<'a, Mode> ManualSession<'a, Mode> {
    pub(super) fn cancellation_token(&self) -> CancellationToken {
        self.core.cancellation.clone()
    }

    pub(super) fn incomplete<A>(
        mut self,
        reason: ManualError,
        retained: Option<A>,
    ) -> Incomplete<A> {
        let reason = self.core.fail(reason);
        let report = Box::new(self.core.recorder.0.snapshot());
        Incomplete {
            reason,
            retained,
            report,
            conversation: Box::new(self.core.conversation),
        }
    }

    /// Detach a storage-stopped session explicitly, preserving live borrowed
    /// dependencies. The old report remains final; this creates a fresh scope.
    pub fn into_memory(mut self) -> Result<Self, ManualError> {
        self.core.conversation = self.core.conversation.into_memory()?;
        self.core.recorder = RunRecorder::new();
        self.core.progress = ProgressOwner::default();
        self.core.conversation = self
            .core
            .conversation
            .with_report(self.core.recorder.0.clone())
            .with_progress(self.core.progress.publisher());
        Ok(self)
    }

    pub fn adopt_pending_write(&mut self) -> Result<(), ManualError> {
        let result = self.core.conversation.adopt_pending_write();
        self.core.result(result)
    }

    pub fn discard_pending_write(&mut self) -> Result<(), ManualError> {
        Ok(self.core.conversation.discard_pending_write()?)
    }

    pub fn report(&self) -> ReportHandle {
        self.core.recorder.0.clone()
    }

    /// Observe this session without driving it. Subscribing after termination
    /// returns a closed receiver; earlier notifications are not replayed.
    pub fn subscribe(&self) -> ProgressReceiver {
        self.subscribe_with_capacity(NonZeroUsize::new(64).unwrap())
    }

    pub fn subscribe_with_capacity(&self, capacity: NonZeroUsize) -> ProgressReceiver {
        self.core.progress.subscribe(capacity)
    }

    pub(super) fn refinement_progress(&self, stage: RefinementStage) {
        if let Some(response) = self.core.conversation.responses().len().checked_sub(1) {
            self.core
                .progress
                .publisher()
                .emit(|| Progress::Refinement { response, stage });
        }
    }

    pub fn conversation(&self) -> &Conversation {
        &self.core.conversation
    }
    pub fn action(&self, action: &PendingAction) -> Result<PendingActionView<'_>, ManualError> {
        Ok(self.core.conversation.action(action)?)
    }
    /// Recover ownership of unresolved actions after a precondition error or a
    /// discarded handle. Reissued handles still validate authoritative state.
    pub fn pending_actions(&self) -> Result<ResponseActions, ManualError> {
        Ok(self.core.conversation.pending_actions()?)
    }
    pub fn call_model(&mut self) -> ModelCall<'_, 'a> {
        let model = self.core.model.clone();
        ModelCall {
            core: &mut self.core,
            model,
            tools: ToolSelection::All,
        }
    }
    pub async fn execute(
        &mut self,
        action: PendingAction,
    ) -> Result<ActionResolution, ManualError> {
        self.core.execute(action).await
    }
    pub fn reject(
        &mut self,
        action: PendingAction,
        feedback: impl Into<String>,
    ) -> Result<ActionResolution, ManualError> {
        self.core.check_active()?;
        let feedback = feedback.into();
        if self.core.conversation.action(&action)?.kind == super::ActionKind::Submission {
            // A text-only run can still receive an unoffered submit call. It
            // must remain explicitly rejectable without enabling acceptance.
            let result = self
                .core
                .conversation
                .revise_submission(&action, feedback.clone());
            self.core.result(result)?;
            Ok(ActionResolution::Rejected(ToolOutput::text(feedback)))
        } else {
            let result = self
                .core
                .conversation
                .reject_tool(&action, ToolOutput::text(feedback));
            self.core.result(result)
        }
    }
    /// Add caller feedback only when no proposal requires a paired result.
    pub fn feedback(&mut self, feedback: impl Into<String>) -> Result<(), ManualError> {
        self.core.check_active()?;
        let result = self
            .core
            .conversation
            .push_user(vec![UserContent::text(feedback)]);
        self.core.result(result)
    }
    pub fn stop(&mut self, reason: impl Into<String>) -> ManualError {
        self.core.fail(ExecutionStop::Stopped(reason.into()).into())
    }
}

impl<T: DeserializeOwned + JsonSchema> ManualSession<'_, Structured<T>> {
    /// Decoding may pair repair feedback; Ready still requires explicit resolve.
    pub fn submission(&mut self, action: &PendingAction) -> Result<Submission<T>, ManualError> {
        self.core.check_active()?;
        match self.core.conversation.submission(action) {
            Err(error @ SubmissionError::Schema(_)) => Err(self.core.fail(error.into())),
            result => {
                let submission = self.core.result(result)?;
                self.refinement_progress(match &submission {
                    Submission::Ready(_) => RefinementStage::Ready,
                    Submission::Repaired { .. } => RefinementStage::Repaired,
                });
                Ok(submission)
            }
        }
    }
    pub fn revise(
        &mut self,
        action: PendingAction,
        feedback: impl Into<String>,
    ) -> Result<(), ManualError> {
        self.core.check_active()?;
        let result = self.core.conversation.revise_submission(&action, feedback);
        self.core.result(result)?;
        self.refinement_progress(RefinementStage::Revised);
        Ok(())
    }
    pub fn accept<A>(&mut self, action: PendingAction, accepted: A) -> Result<A, ManualError> {
        self.complete_submission(action, accepted, super::SubmissionCompletion::Application)
    }

    pub(super) fn complete_submission<A>(
        &mut self,
        action: PendingAction,
        accepted: A,
        kind: super::SubmissionCompletion,
    ) -> Result<A, ManualError> {
        self.core.check_active()?;
        let result = self
            .core
            .conversation
            .accept_submission_with_kind(&action, accepted, kind);
        let accepted = self.core.result(result)?;
        self.core.check_completion()?;
        self.core.recorder.0.finish(RunOutcome::Completed);
        self.refinement_progress(RefinementStage::Accepted);
        self.core.progress.close();
        Ok(accepted)
    }
    pub fn repair_missing_submission(&mut self) -> Result<(), ManualError> {
        self.core.check_active()?;
        let result = self.core.conversation.repair_missing_submission();
        self.core.result(result)?;
        self.refinement_progress(RefinementStage::MissingSubmission);
        Ok(())
    }
}

impl ManualSession<'_, Text> {
    pub fn finish(&mut self) -> Result<String, ManualError> {
        self.core.check_active()?;
        self.core.conversation.require_ready()?;
        let text = self
            .core
            .conversation
            .final_text()
            .ok_or(ManualError::NoText)?;
        let result = self
            .core
            .conversation
            .finish_with_kind(super::HistoryCompletion::Text {
                response: self.core.conversation.responses().len() - 1,
            });
        self.core.result(result)?;
        self.core.check_completion()?;
        self.core.recorder.0.finish(RunOutcome::Completed);
        self.core.progress.close();
        Ok(text)
    }
}

/// Awaiting this builder makes one explicit model invocation, with shared
/// admission and compaction. Resumed manual sessions must recover first.
pub struct ModelCall<'s, 'a> {
    core: &'s mut SessionCore<'a>,
    model: Model<'s>,
    tools: ToolSelection,
}

impl<'s> ModelCall<'s, '_> {
    pub fn model(mut self, model: &Model<'s>) -> Self {
        self.model = model.clone();
        self
    }
    pub fn application_tools(mut self, selection: ToolSelection) -> Self {
        self.tools = selection;
        self
    }
}

impl<'s> IntoFuture for ModelCall<'s, '_> {
    type Output = Result<ResponseActions, ManualError>;
    type IntoFuture = Pin<Box<dyn std::future::Future<Output = Self::Output> + Send + 's>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move { self.core.call_model(&self.model, self.tools).await })
    }
}

pub(super) struct SessionCore<'a> {
    model: Model<'a>,
    conversation: Conversation,
    recorder: RunRecorder,
    progress: ProgressOwner,
    tools: ToolRegistry<'a>,
    budget: Box<dyn Budget + 'a>,
    cancellation: CancellationToken,
    instructions: String,
    submission: Option<ToolDefinition>,
    setup_error: Option<ManualError>,
    tool_hook: Option<Box<dyn ToolCallHook + 'a>>,
    compaction: super::CompactionConfig,
    compaction_configured: bool,
    observer: Option<Box<dyn crate::inference::observation::InferenceObserver + 'a>>,
    exploration_turns: Option<usize>,
    resume_pending: bool,
    resume_response: bool,
}

impl SessionCore<'_> {
    fn result<T, E: Into<ManualError>>(&mut self, result: Result<T, E>) -> Result<T, ManualError> {
        result.map_err(|error| {
            let error = error.into();
            if matches!(error, ManualError::Stop(ExecutionStop::Storage(_))) {
                self.fail(error)
            } else {
                error
            }
        })
    }

    fn fail(&mut self, error: ManualError) -> ManualError {
        if self.resume_pending {
            // Preparation may still own a read-only JSONL source. Do not invent
            // an acknowledged in-memory Stopped record for that source.
            self.conversation.mark_stopped();
            self.recorder.incomplete(&error);
            self.progress.close();
            return error;
        }
        self.conversation
            .configure_history(&self.instructions, self.submission.as_ref());
        let error = match self
            .conversation
            .stop_with_reason(super::report::failure_kind(&error), error.to_string())
        {
            Err(ConversationError::Storage(failure)) => {
                ManualError::Stop(ExecutionStop::Storage(Box::new((*failure).clone())))
            }
            _ => error,
        };
        self.conversation.mark_stopped();
        self.recorder.incomplete(&error);
        self.progress.close();
        error
    }

    fn check_completion(&mut self) -> Result<(), ManualError> {
        // A synchronous writer can observe/cause cancellation while committing.
        // Keep the completed durable evidence, but never return live success
        // after cancellation. Failed writes have already returned as storage stops.
        if self.cancellation.is_cancelled() {
            return Err(self.fail(
                ExecutionStop::Cancelled("execution cancelled during completion".into()).into(),
            ));
        }
        Ok(())
    }

    fn check_active(&mut self) -> Result<(), ManualError> {
        if self.resume_pending {
            return Err(ResumeError::PreparationRequired.into());
        }
        self.conversation
            .configure_history(&self.instructions, self.submission.as_ref());
        if let Some(failure) = self.conversation.history_failure() {
            return Err(ExecutionStop::Storage(Box::new(failure.clone())).into());
        }
        if let Some(error) = self.setup_error.take() {
            return Err(self.fail(error));
        }
        if self.cancellation.is_cancelled() {
            return Err(self.fail(ExecutionStop::Cancelled("execution cancelled".into()).into()));
        }
        if self.conversation.status() != super::ConversationStatus::Active {
            return Err(ConversationError::Terminal.into());
        }
        let result = self.conversation.ensure_history();
        self.result(result)
    }

    fn model_request(&self, selection: ToolSelection) -> ModelRequest {
        let mut tools: Vec<_> = match selection {
            ToolSelection::All => self.tools.definitions().cloned().collect(),
            ToolSelection::None => Vec::new(),
        };
        if let Some(submission) = &self.submission {
            tools.push(submission.clone());
        }
        ModelRequest {
            system: self.instructions.clone(),
            messages: Vec::new(),
            tools,
            schema: None,
            max_tokens: None,
            temperature: None,
        }
    }

    async fn call_model(
        &mut self,
        model: &Model<'_>,
        selection: ToolSelection,
    ) -> Result<ResponseActions, ManualError> {
        self.check_active()?;
        self.conversation.require_ready()?;
        let positions = self.conversation.model_view()?.original_positions();
        self.check_compatibility(model, &positions)?;
        let selection = if self.exploration_turns == Some(0) {
            ToolSelection::None
        } else {
            selection
        };
        let mut request = self.model_request(selection);
        let application_tools_offered =
            matches!(selection, ToolSelection::All) && !self.tools.is_empty();
        let tools = request.tools.clone();
        // Reject unsupported main requests before a summary can consume budget
        // or commit a different view. Invocation repeats the same preflight.
        let publisher = self.progress.publisher();
        model.validate_request(&request, Self::streams_tokens(model, &publisher, true))?;
        let (messages, input, max_tokens) = self.prepare_model_view(model, &tools).await?;
        request.messages = messages;
        request.max_tokens = max_tokens;
        let result = Self::invoke(
            model,
            request,
            self.execution(&input),
            self.progress.publisher(),
            true,
            self.observations(),
        )
        .await;
        let output = match result {
            Ok(output) => output,
            Err(error @ ModelError::Unsupported { .. }) => return Err(error.into()),
            Err(ModelError::Provider(error)) => {
                let error = match error.into_execution_stop() {
                    Ok(stop) => stop.into(),
                    Err(error) => ModelError::Provider(error).into(),
                };
                return Err(self.fail(error));
            }
            Err(error) => {
                return Err(self.fail(error.into()));
            }
        };
        let result =
            self.conversation
                .record_response_from(output.output, tools, Some(output.origin));
        let actions = self.result(result)?;
        if application_tools_offered
            && actions.actions.iter().any(|action| {
                self.conversation
                    .action(action)
                    .is_ok_and(|view| view.kind == super::ActionKind::Tool)
            })
            && let Some(turns) = &mut self.exploration_turns
        {
            *turns = turns.saturating_sub(1);
        }
        Ok(actions)
    }

    fn observations(&self) -> Observations<'_> {
        self.observer
            .as_deref()
            .map(Observations::new)
            .unwrap_or_default()
    }

    fn execution<'s>(&'s self, input: &'s super::RequestInput) -> ExecutionControl<'s> {
        ExecutionControl::new(&*self.budget)
            .with_cancellation(&self.cancellation)
            .with_report(&self.recorder.0)
            .with_input(input)
    }

    fn streams_tokens(
        model: &Model<'_>,
        publisher: &super::progress::ProgressPublisher,
        stream: bool,
    ) -> bool {
        stream && publisher.has_subscribers() && model.capabilities().streaming
    }

    async fn invoke(
        model: &Model<'_>,
        request: ModelRequest,
        execution: ExecutionControl<'_>,
        publisher: super::progress::ProgressPublisher,
        stream: bool,
        observations: Observations<'_>,
    ) -> Result<super::model::InvocationOutput, ModelError> {
        if Self::streams_tokens(model, &publisher, stream) {
            // Drive both futures here: borrowed adapters need no spawned task,
            // and an undrained public receiver cannot backpressure this channel.
            const TOKEN_BUFFER: usize = 16;
            let (tokens, mut receiver) = tokio::sync::mpsc::channel(TOKEN_BUFFER);
            let invocation =
                model.invoke_with_origin(request, Some(tokens), observations, execution);
            tokio::pin!(invocation);
            let emit = |token| {
                publisher.emit(|| match token {
                    StreamToken::Text(text) => Progress::Text {
                        model: model.config().as_str(),
                        text,
                    },
                    StreamToken::Reasoning(text) => Progress::Reasoning {
                        model: model.config().as_str(),
                        text,
                    },
                })
            };
            let mut open = true;
            let result = loop {
                tokio::select! {
                    biased;
                    result = &mut invocation => break result,
                    token = receiver.recv(), if open => match token {
                        Some(token) => emit(token),
                        None => open = false,
                    },
                }
            };
            receiver.close();
            // A custom adapter may retain cloned senders after returning. Never
            // wait for them, or let post-return producers extend this drain.
            for _ in 0..TOKEN_BUFFER {
                match receiver.try_recv() {
                    Ok(token) => emit(token),
                    Err(_) => break,
                }
            }
            result
        } else {
            model
                .invoke_with_origin(request, None, observations, execution)
                .await
        }
    }

    async fn execute(&mut self, action: PendingAction) -> Result<ActionResolution, ManualError> {
        self.check_active()?;
        // Admission belongs to dispatch, after asynchronous policy has finished.
        let execution = ExecutionControl::new(&*self.budget)
            .with_cancellation(&self.cancellation)
            .with_report(&self.recorder.0);
        match self
            .conversation
            .execute_tool_with_hook(
                &action,
                &mut self.tools,
                execution,
                self.tool_hook
                    .as_mut()
                    .map(|hook| &mut **hook as &mut dyn ToolCallHook),
            )
            .await
        {
            Err(ToolDispatchError::Stop(stop)) => Err(self.fail(stop.into())),
            Err(error @ ToolDispatchError::ToolHook(_)) => Err(self.fail(error.into())),
            result => self.result(result),
        }
    }

    fn check_compatibility(
        &self,
        model: &Model<'_>,
        positions: &[usize],
    ) -> Result<(), ManualError> {
        let target = model.config().as_str();
        let incompatible = |reason| ManualError::Incompatible {
            model: target.clone(),
            reason,
        };
        let has_tool_history = self
            .conversation
            .responses()
            .iter()
            .any(|r| positions.contains(&r.message_position()) && !r.actions().is_empty());
        if has_tool_history && !model.capabilities().tools {
            return Err(incompatible("destination does not support tool exchanges"));
        }
        for record in self
            .conversation
            .responses()
            .iter()
            .filter(|r| positions.contains(&r.message_position()))
        {
            if record.origin() == Some(target.as_str()) {
                continue;
            }
            if !record
                .output()
                .content
                .iter()
                .all(super::model::portable_assistant_content)
            {
                return Err(incompatible(
                    "cross-model replay of signed, opaque, dual-ID or multimodal content is unsupported",
                ));
            }
            for action in record.actions() {
                if action.resolution().is_some_and(|resolution| {
                    resolution
                        .output()
                        .content
                        .iter()
                        .any(|content| matches!(content, ToolResultContent::Image(_)))
                }) {
                    return Err(incompatible(
                        "cross-model replay of image tool results is unsupported",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod history_tests;

mod compaction;

#[cfg(test)]
mod compaction_tests;

mod resume;
pub use resume::ResumeError;
