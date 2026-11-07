//! Configured runs and caller-driven execution over the shared SDK core.

use std::{future::IntoFuture, marker::PhantomData, pin::Pin};

use rig_core::completion::{
    AssistantContent,
    message::{ToolResultContent, UserContent},
    request::ToolDefinition,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::inference::observation::Observations;

use super::{
    ActionResolution, Budget, Conversation, ConversationError, DefaultBudget, ExecutionControl,
    ExecutionStop, Model, ModelError, ModelRequest, PendingAction, PendingActionView,
    ResponseActions, Submission, SubmissionError, Tool, ToolCallHook, ToolDecision,
    ToolDispatchError, ToolOutput, ToolRegistrationError, ToolRegistry, hook::CallbackHook,
    submission_tool,
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
        Run {
            core: SessionCore {
                model: self.model.clone(),
                conversation: Conversation::new(input),
                tools: ToolRegistry::default(),
                budget: Box::new(DefaultBudget::default()),
                cancellation: CancellationToken::new(),
                instructions: String::new(),
                submission: None,
                setup_error: None,
                tool_hook: None,
                origins: Vec::new(),
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
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.core.instructions = instructions.into();
        self
    }
    pub fn tool(mut self, tool: impl Tool + 'a) -> Self {
        if self.core.setup_error.is_none() {
            self.core.setup_error = self.core.tools.register(tool).err();
        }
        self
    }
    pub fn tools(mut self, tools: ToolRegistry<'a>) -> Self {
        if self.core.setup_error.is_none() {
            self.core.setup_error = self.core.tools.extend(tools).err();
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
    Setup(#[from] ToolRegistrationError),
    #[error(transparent)]
    Conversation(#[from] ConversationError),
    #[error(transparent)]
    Submission(#[from] SubmissionError),
    #[error(transparent)]
    Dispatch(#[from] ToolDispatchError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Stop(#[from] ExecutionStop),
    #[error("cannot replay history to {model}: {reason}")]
    Incompatible { model: String, reason: &'static str },
    #[error("application inspector failed: {0}")]
    Inspector(String),
    #[error("application tool hook failed: {0}")]
    ToolHook(String),
    #[error("the current response has no final text")]
    NoText,
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

    pub(super) fn into_stopped_conversation(mut self) -> Conversation {
        self.core.conversation.stop();
        self.core.conversation
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
            self.core
                .conversation
                .revise_submission(&action, feedback.clone())?;
            Ok(ActionResolution::Rejected(ToolOutput::text(feedback)))
        } else {
            Ok(self
                .core
                .conversation
                .reject_tool(&action, ToolOutput::text(feedback))?)
        }
    }
    /// Add caller feedback only when no proposal requires a paired result.
    pub fn feedback(&mut self, feedback: impl Into<String>) -> Result<(), ManualError> {
        self.core.check_active()?;
        Ok(self
            .core
            .conversation
            .push_user(vec![UserContent::text(feedback)])?)
    }
    pub fn stop(&mut self, reason: impl Into<String>) -> ManualError {
        self.core.conversation.stop();
        ExecutionStop::Stopped(reason.into()).into()
    }
}

impl<T: DeserializeOwned + JsonSchema> ManualSession<'_, Structured<T>> {
    /// Decoding may pair repair feedback; Ready still requires explicit resolve.
    pub fn submission(&mut self, action: &PendingAction) -> Result<Submission<T>, ManualError> {
        self.core.check_active()?;
        match self.core.conversation.submission(action) {
            Err(error @ SubmissionError::Schema(_)) => {
                self.core.conversation.stop();
                Err(error.into())
            }
            result => Ok(result?),
        }
    }
    pub fn revise(
        &mut self,
        action: PendingAction,
        feedback: impl Into<String>,
    ) -> Result<(), ManualError> {
        self.core.check_active()?;
        Ok(self
            .core
            .conversation
            .revise_submission(&action, feedback)?)
    }
    pub fn accept<A>(&mut self, action: PendingAction, accepted: A) -> Result<A, ManualError> {
        self.core.check_active()?;
        Ok(self
            .core
            .conversation
            .accept_submission(&action, accepted)?)
    }
    pub fn repair_missing_submission(&mut self) -> Result<(), ManualError> {
        self.core.check_active()?;
        Ok(self.core.conversation.repair_missing_submission()?)
    }
}

impl ManualSession<'_, Text> {
    pub fn finish(&mut self) -> Result<String, ManualError> {
        self.core.check_active()?;
        self.core.conversation.request_messages()?;
        let response = self
            .core
            .conversation
            .responses()
            .last()
            .ok_or(ManualError::NoText)?;
        // A tool-only response is not a final answer even after its tools resolve.
        if !response.actions().is_empty() {
            return Err(ManualError::NoText);
        }
        let text: String = response
            .output()
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        if text.is_empty() {
            return Err(ManualError::NoText);
        }
        self.core.conversation.finish()?;
        Ok(text)
    }
}

/// Awaiting this builder makes one explicit model invocation. Automatic retry,
/// compaction, progress and recovery are integrated by their dedicated tasks.
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
    tools: ToolRegistry<'a>,
    budget: Box<dyn Budget + 'a>,
    cancellation: CancellationToken,
    instructions: String,
    submission: Option<ToolDefinition>,
    setup_error: Option<ToolRegistrationError>,
    tool_hook: Option<Box<dyn ToolCallHook + 'a>>,
    // One origin for each immutable response; never inferred from mutable selection.
    origins: Vec<String>,
}

impl SessionCore<'_> {
    fn check_active(&mut self) -> Result<(), ManualError> {
        if let Some(error) = self.setup_error.take() {
            self.conversation.stop();
            return Err(error.into());
        }
        if self.cancellation.is_cancelled() {
            self.conversation.stop();
            return Err(ExecutionStop::Cancelled("execution cancelled".into()).into());
        }
        if self.conversation.status() != super::ConversationStatus::Active {
            return Err(ConversationError::Terminal.into());
        }
        Ok(())
    }

    async fn call_model(
        &mut self,
        model: &Model<'_>,
        selection: ToolSelection,
    ) -> Result<ResponseActions, ManualError> {
        self.check_active()?;
        self.conversation.request_messages()?;
        self.check_compatibility(model)?;
        let mut tools: Vec<_> = match selection {
            ToolSelection::All => self.tools.definitions().cloned().collect(),
            ToolSelection::None => Vec::new(),
        };
        if let Some(submission) = &self.submission {
            tools.push(submission.clone());
        }
        let request = ModelRequest {
            system: self.instructions.clone(),
            messages: self.conversation.request_messages()?.to_vec(),
            tools: tools.clone(),
            schema: None,
            max_tokens: None,
            temperature: None,
        };
        let execution = ExecutionControl::new(&*self.budget).with_cancellation(&self.cancellation);
        let output = match model
            .invoke_controlled(request, None, Observations::default(), execution)
            .await
        {
            Ok(output) => output,
            Err(error @ ModelError::Unsupported { .. }) => return Err(error.into()),
            Err(ModelError::Provider(error)) => {
                self.conversation.stop();
                return Err(match error.into_execution_stop() {
                    Ok(stop) => stop.into(),
                    Err(error) => ModelError::Provider(error).into(),
                });
            }
            Err(error) => {
                self.conversation.stop();
                return Err(error.into());
            }
        };
        let actions = self.conversation.record_response(output, tools)?;
        self.origins.push(model.config().as_str());
        Ok(actions)
    }

    async fn execute(&mut self, action: PendingAction) -> Result<ActionResolution, ManualError> {
        self.check_active()?;
        self.conversation.require_tool_proposal(&action)?;
        let view = self.conversation.action(&action)?;
        // Unoffered or missing tools follow ordinary paired rejection without
        // consulting application policy. A hook cannot authorize either one.
        let eligible = view
            .offered_tools
            .iter()
            .any(|tool| tool.name == view.call.function.name)
            && self.tools.get_mut(&view.call.function.name).is_some();
        if eligible && let Some(hook) = &mut self.tool_hook {
            let result = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => Err(ManualError::Stop(
                    ExecutionStop::Cancelled("execution cancelled during tool hook".into())
                )),
                result = hook.decide(view.call) => result.map_err(|error| ManualError::ToolHook(error.to_string())),
            };
            // A hook may cancel the scope synchronously while returning a
            // decision, including Reject. Never pair or execute after that stop.
            self.check_active()?;
            match result {
                Ok(ToolDecision::Execute) => {}
                Ok(ToolDecision::Reject(feedback)) => {
                    return Ok(self
                        .conversation
                        .reject_tool(&action, ToolOutput::text(feedback))?);
                }
                Ok(ToolDecision::Stop(reason)) => {
                    self.conversation.stop();
                    return Err(ExecutionStop::Stopped(reason).into());
                }
                Err(error) => {
                    self.conversation.stop();
                    return Err(error);
                }
            }
        }
        // Admission belongs to dispatch, after asynchronous policy has finished.
        let execution = ExecutionControl::new(&*self.budget).with_cancellation(&self.cancellation);
        match self
            .conversation
            .execute_tool(&action, &mut self.tools, execution)
            .await
        {
            Err(ToolDispatchError::Stop(stop)) => Err(stop.into()),
            result => Ok(result?),
        }
    }

    fn check_compatibility(&self, model: &Model<'_>) -> Result<(), ManualError> {
        let target = model.config().as_str();
        let incompatible = |reason| ManualError::Incompatible {
            model: target.clone(),
            reason,
        };
        let has_tool_history = self
            .conversation
            .responses()
            .iter()
            .any(|r| !r.actions().is_empty());
        if has_tool_history && !model.capabilities().tools {
            return Err(incompatible("destination does not support tool exchanges"));
        }
        for (record, origin) in self.conversation.responses().iter().zip(&self.origins) {
            if origin == &target {
                continue;
            }
            for content in &record.output().content {
                match content {
                    AssistantContent::Text(_) => {}
                    AssistantContent::ToolCall(call)
                        if call.signature.is_none()
                            && call.additional_params.is_none()
                            && call.provider.as_ref().is_none_or(|id| id.item_id.is_none()) => {}
                    _ => {
                        return Err(incompatible(
                            "cross-model replay of signed, opaque, dual-ID or multimodal content is unsupported",
                        ));
                    }
                }
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
