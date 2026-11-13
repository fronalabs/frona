//! Individual and bounded batch dispatch shared by execution drivers.

use futures::{StreamExt, stream::FuturesUnordered};
use std::{collections::VecDeque, num::NonZeroUsize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    ActionKind, ActionResolution, ActionStatus, Conversation, ConversationError,
    ConversationStatus, ExecutionControl, ExecutionStop, FailureKind, PendingAction,
    ResponseActions, ToolCallHook, ToolContext, ToolDecision, ToolError, ToolOutput, ToolRegistry,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecoveryError {
    #[error("history is already completed; recovery cannot recreate an application result")]
    AlreadyCompleted,
    #[error("interrupted tool '{name}' has no restored implementation")]
    MissingTool { name: String },
    #[error("interrupted tool '{name}' was not offered for its recorded request")]
    UnofferedTool { name: String },
    #[error("recovery requires a new, unfinished execution report")]
    FinalizedReport,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolDispatchError {
    #[error(transparent)]
    Conversation(ConversationError),
    #[error("submission actions require submission resolution, not tool dispatch")]
    Submission,
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
    #[error("application tool hook failed: {0}")]
    ToolHook(String),
    #[error(transparent)]
    Stop(#[from] ExecutionStop),
}

impl From<ConversationError> for ToolDispatchError {
    fn from(error: ConversationError) -> Self {
        match error {
            ConversationError::Storage(failure) => {
                Self::Stop(ExecutionStop::Storage(Box::new((*failure).clone())))
            }
            error => Self::Conversation(error),
        }
    }
}

impl Conversation {
    /// Execute one proposed ordinary tool. Each call receives fresh admission.
    /// Unknown/unoffered tools receive paired rejection without invocation.
    pub async fn execute_tool(
        &mut self,
        handle: &PendingAction,
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<ActionResolution, ToolDispatchError> {
        self.dispatch_tool(handle, tools, execution, None, false)
            .await
    }

    /// Execute a response batch with bounded concurrency. Mutable instances are
    /// serialized individually; distinct tools overlap. Returned results retain
    /// proposal order, while history records each completion as it arrives.
    pub async fn execute_tools(
        &mut self,
        actions: &[PendingAction],
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
        maximum: NonZeroUsize,
    ) -> Result<Vec<ActionResolution>, ToolDispatchError> {
        self.execute_tools_with_hook(actions, tools, execution, maximum, None)
            .await
    }

    pub(crate) async fn execute_tools_with_hook(
        &mut self,
        actions: &[PendingAction],
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
        maximum: NonZeroUsize,
        hook: Option<&mut dyn ToolCallHook>,
    ) -> Result<Vec<ActionResolution>, ToolDispatchError> {
        let tools = tools.locked();
        let calls = actions
            .iter()
            .map(|action| {
                let name = &self.action(action)?.call.function.name;
                Ok((action, tools.get(name.as_str())))
            })
            .collect::<Result<Vec<_>, ToolDispatchError>>()?;
        self.dispatch_batch(calls, execution, maximum, hook).await
    }

    /// Execute call-local tool instances under the same batch scheduler. This is
    /// useful for hosts whose tools carry per-invocation persistence state.
    /// Each instance's definition must match its proposed tool name.
    pub async fn execute_tool_instances(
        &mut self,
        calls: &mut [(&PendingAction, Box<dyn crate::Tool + '_>)],
        execution: ExecutionControl<'_>,
        maximum: NonZeroUsize,
    ) -> Result<Vec<ActionResolution>, ToolDispatchError> {
        for (action, tool) in calls.iter() {
            if self.action(action)?.call.function.name != tool.definition().name {
                return Err(ToolDispatchError::ToolHook(
                    "tool instance does not match proposal".into(),
                ));
            }
        }
        let tools: Vec<_> = calls
            .iter_mut()
            .map(|(action, tool)| (*action, Mutex::new(tool.as_mut())))
            .collect();
        self.dispatch_batch(
            tools
                .iter()
                .map(|(action, tool)| (*action, Some(tool)))
                .collect(),
            execution,
            maximum,
            None,
        )
        .await
    }

    async fn dispatch_batch<'tool>(
        &mut self,
        calls: Vec<(
            &PendingAction,
            Option<&Mutex<&mut (dyn crate::Tool + 'tool)>>,
        )>,
        execution: ExecutionControl<'_>,
        maximum: NonZeroUsize,
        hook: Option<&mut dyn ToolCallHook>,
    ) -> Result<Vec<ActionResolution>, ToolDispatchError> {
        let mut results = vec![None; calls.len()];
        let conversation = Mutex::new(self);
        let hook = Mutex::new(hook);
        let mut pending: VecDeque<_> = calls.into_iter().enumerate().collect();
        let admission = CancellationToken::new();
        let mut running = FuturesUnordered::new();
        let mut failure = None;
        loop {
            while failure.is_none() && running.len() < maximum.get() {
                // Reserve an available instance before enqueueing its future.
                // Calls waiting for that instance do not occupy running slots.
                let next = pending
                    .iter()
                    .enumerate()
                    .find_map(|(position, (_, (_, tool)))| match *tool {
                        Some(tool) => tool.try_lock().ok().map(|guard| (position, Some(guard))),
                        None => Some((position, None)),
                    });
                let Some((position, mut tool)) = next else {
                    break;
                };
                let (index, (action, _)) = pending.remove(position).unwrap();
                let conversation = &conversation;
                let hook = &hook;
                let admission = &admission;
                running.push(async move {
                    let result = Self::dispatch_tool_inner(
                        conversation,
                        action,
                        tool.as_mut()
                            .map(|tool| &mut ***tool as &mut (dyn crate::Tool + 'tool)),
                        execution,
                        hook,
                        admission,
                        false,
                    )
                    .await;
                    (index, result)
                });
            }
            let Some((index, result)) = running.next().await else {
                break;
            };
            match result {
                Ok(result) => results[index] = result,
                Err(error) => {
                    // Budget refusal does not revoke already admitted calls.
                    // Other terminal failures abandon in-flight work, retaining
                    // Started records and settling permits through their guards.
                    let drain = matches!(
                        error,
                        ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_))
                    );
                    admission.cancel();
                    if failure.is_none() || !drain {
                        failure = Some(error);
                    }
                    if !drain {
                        break;
                    }
                }
            }
        }
        drop(running);
        let conversation = conversation.into_inner();
        Self::finish_dispatch(
            conversation,
            match failure {
                Some(error) => Err(error),
                None => Ok(results.into_iter().map(Option::unwrap).collect()),
            },
        )
    }

    /// Explicitly retry interrupted ordinary calls, in proposal order, using
    /// freshly supplied tools and shared controls. Tools must be safe to repeat:
    /// a prior attempt may already have completed an external effect.
    ///
    /// Resolved/rejected calls never rerun. Unstarted proposals and submissions
    /// are returned for caller decisions. This neither invokes a model nor
    /// restores inspector state or budget consumption. A supplied report must
    /// belong to a new execution scope; old report/progress bindings are detached.
    pub async fn recover_tools(
        &mut self,
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<ResponseActions, ToolDispatchError> {
        self.recover_tools_inner(tools, execution, None).await
    }

    /// Recovery with restored application policy. Reject/Stop preserves an
    /// interrupted action and stops recovery; it cannot undo a prior effect and
    /// therefore never writes rejection feedback for an interrupted call.
    pub async fn recover_tools_with_hook(
        &mut self,
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
        hook: &mut dyn ToolCallHook,
    ) -> Result<ResponseActions, ToolDispatchError> {
        self.recover_tools_inner(tools, execution, Some(hook)).await
    }

    async fn recover_tools_inner(
        &mut self,
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
        mut hook: Option<&mut dyn ToolCallHook>,
    ) -> Result<ResponseActions, ToolDispatchError> {
        // These checks must precede reactivation, observer rebinding, or writes.
        if let Some(failure) = self.history_failure() {
            return Err(ExecutionStop::Storage(Box::new(failure.clone())).into());
        }
        self.require_storage_ready()?;
        if self.pending_write().is_some() {
            return Err(ConversationError::PendingWrite.into());
        }
        if self.status() == ConversationStatus::Finished {
            return Err(RecoveryError::AlreadyCompleted.into());
        }
        if execution
            .report_handle()
            .is_some_and(|r| r.snapshot().outcome.is_some())
        {
            return Err(RecoveryError::FinalizedReport.into());
        }
        self.activate_recovery(execution.report_handle());
        self.check_dispatch_cancellation(execution)?;
        let pending = self.pending_actions()?;
        // Preflight all interrupted dependencies before any retry can have an effect.
        for action in &pending.actions {
            if self.action(action)?.status == ActionStatus::Started
                && let Err(error) = self.require_recovery_tool(action, tools)
            {
                return Err(self.stop_dispatch_error(error));
            }
        }
        for action in &pending.actions {
            if self.action(action)?.status != ActionStatus::Started {
                continue;
            }
            let hook = hook
                .as_mut()
                .map(|hook| &mut **hook as &mut dyn ToolCallHook);
            self.dispatch_tool(action, tools, execution, hook, true)
                .await?;
        }
        self.pending_actions().map_err(Into::into)
    }

    fn require_recovery_tool(
        &self,
        handle: &PendingAction,
        tools: &mut ToolRegistry<'_>,
    ) -> Result<(), ToolDispatchError> {
        self.next_invocation(handle, true)?;
        let view = self.action(handle)?;
        let name = &view.call.function.name;
        if !view
            .offered_tools
            .iter()
            .any(|definition| definition.name == *name)
        {
            return Err(RecoveryError::UnofferedTool {
                name: name.to_string(),
            }
            .into());
        }
        if tools.get_mut(name).is_none() {
            return Err(RecoveryError::MissingTool {
                name: name.to_string(),
            }
            .into());
        }
        Ok(())
    }

    pub(crate) async fn execute_tool_with_hook(
        &mut self,
        handle: &PendingAction,
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
        hook: Option<&mut dyn ToolCallHook>,
    ) -> Result<ActionResolution, ToolDispatchError> {
        self.dispatch_tool(handle, tools, execution, hook, false)
            .await
    }

    async fn dispatch_tool(
        &mut self,
        handle: &PendingAction,
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
        hook: Option<&mut dyn ToolCallHook>,
        recovery: bool,
    ) -> Result<ActionResolution, ToolDispatchError> {
        let conversation = Mutex::new(self);
        let hook = Mutex::new(hook);
        let tool = tools.get_mut(
            &conversation
                .lock()
                .await
                .action(handle)?
                .call
                .function
                .name
                .clone(),
        );
        let result = Self::dispatch_tool_inner(
            &conversation,
            handle,
            tool,
            execution,
            &hook,
            &CancellationToken::new(),
            recovery,
        )
        .await
        .map(|result| result.expect("single-call admission stays open"));
        let conversation = conversation.into_inner();
        Self::finish_dispatch(conversation, result)
    }

    fn finish_dispatch<T>(
        conversation: &mut Conversation,
        result: Result<T, ToolDispatchError>,
    ) -> Result<T, ToolDispatchError> {
        result.map_err(|error| match error {
            ToolDispatchError::Stop(_) | ToolDispatchError::ToolHook(_) => {
                conversation.stop_dispatch_error(error)
            }
            error => error,
        })
    }

    async fn dispatch_tool_inner<'tool>(
        conversation: &Mutex<&mut Conversation>,
        handle: &PendingAction,
        tool: Option<&mut (dyn crate::Tool + 'tool)>,
        execution: ExecutionControl<'_>,
        hook: &Mutex<Option<&mut dyn ToolCallHook>>,
        admission: &CancellationToken,
        recovery: bool,
    ) -> Result<Option<ActionResolution>, ToolDispatchError> {
        if admission.is_cancelled() {
            return Ok(None);
        }
        let mut state = conversation.lock().await;
        state.require_storage_ready()?;
        if recovery {
            state.next_invocation(handle, true)?;
        } else {
            state.require_tool_proposal(handle)?;
        }
        execution.check_cancellation().map_err(|error| {
            ToolDispatchError::Stop(
                error
                    .into_execution_stop()
                    .expect("cancellation is terminal"),
            )
        })?;
        let view = state.action(handle)?;
        let call = view.call.clone();
        let name = call.function.name.clone();
        if !view
            .offered_tools
            .iter()
            .any(|definition| definition.name == name)
        {
            return state
                .reject_tool(
                    handle,
                    ToolOutput::text(format!("Tool '{name}' was not offered for this request.")),
                )
                .map(Some);
        }
        let Some(tool) = tool else {
            return state
                .reject_tool(
                    handle,
                    ToolOutput::text(format!("Tool '{name}' is not registered.")),
                )
                .map(Some);
        };
        drop(state);
        {
            let mut hook = hook.lock().await;
            if let Some(hook) = hook.as_deref_mut() {
                let decision = tokio::select! {
                    biased;
                    _ = admission.cancelled() => return Ok(None),
                    _ = execution.cancelled() => Err(ToolDispatchError::Stop(
                        ExecutionStop::Cancelled("execution cancelled during tool hook".into())
                    )),
                    result = hook.decide(&call) => result.map_err(|e| ToolDispatchError::ToolHook(e.to_string())),
                };
                // Synchronously ready policy may cancel while returning Reject.
                execution.check_cancellation().map_err(|error| {
                    ToolDispatchError::Stop(
                        error
                            .into_execution_stop()
                            .expect("cancellation is terminal"),
                    )
                })?;
                match decision {
                    Ok(ToolDecision::Execute) => {}
                    Ok(ToolDecision::Reject(feedback)) if !recovery => {
                        return conversation
                            .lock()
                            .await
                            .reject_tool(handle, ToolOutput::text(feedback))
                            .map(Some);
                    }
                    Ok(ToolDecision::Reject(reason) | ToolDecision::Stop(reason)) => {
                        return Err(ExecutionStop::Stopped(reason).into());
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if admission.is_cancelled() {
            return Ok(None);
        }
        // Admission occurs only after policy. Both initial and recovery attempts
        // use this one dispatch boundary, including nested controls and storage.
        let execution = execution.with_action(conversation.lock().await.action_position(handle)?);
        let context = ToolContext::for_execution(call.id.to_string(), execution);
        let result = execution
            .with_cancellation(&context.cancellation)
            .tool_attempt(&name, || async {
                conversation
                    .lock()
                    .await
                    .start_invocation(handle, recovery)
                    .map_err(|error| match error {
                        ConversationError::Storage(failure) => {
                            ToolError::Stop(ExecutionStop::Storage(Box::new((*failure).clone())))
                        }
                        error => ToolError::failed(error),
                    })?;
                execution.check_cancellation()?;
                tool.call(
                    crate::inference::submission::call_arguments(&call.function),
                    &context,
                )
                .await
            })
            .await;
        let mut state = conversation.lock().await;
        if let Some(failure) = state.history_failure() {
            return Err(ExecutionStop::Storage(Box::new(failure.clone())).into());
        }
        let resolution = match result {
            Ok(output) => ActionResolution::Result(output),
            Err(ToolError::Feedback(output)) => ActionResolution::Failed(output),
            Err(ToolError::Failed(error)) => {
                ActionResolution::Failed(ToolOutput::text(error.to_string()))
            }
            Err(ToolError::Stop(stop)) => return Err(stop.into()),
        };
        state.resolve(handle, resolution.clone())?;
        execution.check_cancellation().map_err(|error| {
            ToolDispatchError::Stop(
                error
                    .into_execution_stop()
                    .expect("cancellation is terminal"),
            )
        })?;
        Ok(Some(resolution))
    }

    fn check_dispatch_cancellation(
        &mut self,
        execution: ExecutionControl<'_>,
    ) -> Result<(), ToolDispatchError> {
        if let Err(error) = execution.check_cancellation() {
            return Err(self.stop_dispatch_error(
                error
                    .into_execution_stop()
                    .expect("cancellation is terminal")
                    .into(),
            ));
        }
        Ok(())
    }

    fn stop_dispatch_error(&mut self, error: ToolDispatchError) -> ToolDispatchError {
        let category = match &error {
            ToolDispatchError::Stop(stop) => stop.failure_kind(),
            ToolDispatchError::Recovery(_) => FailureKind::Setup,
            ToolDispatchError::ToolHook(_) => FailureKind::ToolHook,
            _ => FailureKind::Protocol,
        };
        match self.stop_with_reason(category, error.to_string()) {
            Err(error) => error.into(),
            Ok(()) => error,
        }
    }

    /// Pair rejection feedback without invoking a tool or charging its allowance.
    /// This accepts only an unstarted ordinary proposal; submissions have their
    /// own typed resolution path. Rejection itself needs no execution budget.
    pub fn reject_tool(
        &mut self,
        handle: &PendingAction,
        feedback: ToolOutput,
    ) -> Result<ActionResolution, ToolDispatchError> {
        self.require_tool_proposal(handle)?;
        let resolution = ActionResolution::Rejected(feedback);
        self.resolve(handle, resolution.clone())?;
        Ok(resolution)
    }

    /// Inspect a validated, unstarted ordinary proposal before host-side writes.
    /// This checks canonical identity, state and pending storage obligations; it
    /// does not authorize the tool or reserve admission. Execution revalidates.
    pub fn tool_proposal(
        &self,
        handle: &PendingAction,
    ) -> Result<crate::PendingActionView<'_>, ToolDispatchError> {
        self.require_tool_proposal(handle)?;
        Ok(self.action(handle)?)
    }

    /// Inspect any unstarted proposal, including a submission the host may reject.
    /// This validates identity and storage state before required host writes.
    pub fn action_proposal(
        &self,
        handle: &PendingAction,
    ) -> Result<crate::PendingActionView<'_>, ToolDispatchError> {
        self.require_action_proposal(handle)?;
        Ok(self.action(handle)?)
    }

    /// Stop with a typed host execution failure, retaining unfinished actions.
    /// A failed required stop write takes precedence over the supplied reason.
    /// The returned error is the authoritative stop; this never completes work.
    pub fn stop_execution(&mut self, stop: ExecutionStop) -> ToolDispatchError {
        self.stop_dispatch_error(stop.into())
    }

    pub(crate) fn require_tool_proposal(
        &self,
        handle: &PendingAction,
    ) -> Result<(), ToolDispatchError> {
        self.require_action_proposal(handle)?;
        if self.action(handle)?.kind != ActionKind::Tool {
            return Err(ToolDispatchError::Submission);
        }
        Ok(())
    }

    fn require_action_proposal(&self, handle: &PendingAction) -> Result<(), ToolDispatchError> {
        self.require_storage_ready()?;
        if let Some(failure) = self.history_failure() {
            return Err(ExecutionStop::Storage(Box::new(failure.clone())).into());
        }
        if self.pending_write().is_some() {
            return Err(ConversationError::PendingWrite.into());
        }
        let view = self.action(handle)?;
        if view.status == ActionStatus::Started {
            return Err(ConversationError::AlreadyStarted.into());
        }
        Ok(())
    }
}
