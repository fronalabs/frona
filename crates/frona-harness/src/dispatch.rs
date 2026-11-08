//! Individual action dispatch shared by manual, automatic, and recovery drivers.

use super::{
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
            return Err(RecoveryError::UnofferedTool { name: name.clone() }.into());
        }
        if tools.get_mut(name).is_none() {
            return Err(RecoveryError::MissingTool { name: name.clone() }.into());
        }
        Ok(())
    }

    pub(super) async fn execute_tool_with_hook(
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
        self.require_storage_ready()?;
        if recovery {
            self.require_recovery_tool(handle, tools)?;
        } else {
            self.require_tool_proposal(handle)?;
        }
        self.check_dispatch_cancellation(execution)?;
        let view = self.action(handle)?;
        let call = view.call.clone();
        let name = call.function.name.clone();
        if !view
            .offered_tools
            .iter()
            .any(|definition| definition.name == name)
        {
            return self.reject_tool(
                handle,
                ToolOutput::text(format!("Tool '{name}' was not offered for this request.")),
            );
        }
        let Some(tool) = tools.get_mut(&name) else {
            return self.reject_tool(
                handle,
                ToolOutput::text(format!("Tool '{name}' is not registered.")),
            );
        };
        if let Some(hook) = hook {
            let decision = tokio::select! {
                biased;
                _ = execution.cancelled() => Err(ToolDispatchError::Stop(
                    ExecutionStop::Cancelled("execution cancelled during tool hook".into())
                )),
                result = hook.decide(&call) => result.map_err(|e| ToolDispatchError::ToolHook(e.to_string())),
            };
            // Synchronously ready policy may cancel while returning Reject.
            self.check_dispatch_cancellation(execution)?;
            match decision {
                Ok(ToolDecision::Execute) => {}
                Ok(ToolDecision::Reject(feedback)) if !recovery => {
                    return self.reject_tool(handle, ToolOutput::text(feedback));
                }
                Ok(ToolDecision::Reject(reason) | ToolDecision::Stop(reason)) => {
                    return Err(self.stop_dispatch_error(ExecutionStop::Stopped(reason).into()));
                }
                Err(error) => return Err(self.stop_dispatch_error(error)),
            }
        }
        // Admission occurs only after policy. Both initial and recovery attempts
        // use this one dispatch boundary, including nested controls and storage.
        let execution = execution.with_action(self.action_position(handle)?);
        let context = ToolContext::for_execution(call.id.as_str().into(), execution);
        let result = execution
            .with_cancellation(&context.cancellation)
            .tool_attempt(&name, || async {
                self.start_invocation(handle, recovery)
                    .map_err(|error| match error {
                        ConversationError::Storage(failure) => {
                            ToolError::Stop(ExecutionStop::Storage(Box::new((*failure).clone())))
                        }
                        error => ToolError::failed(error),
                    })?;
                execution.check_cancellation()?;
                tool.call(call.function.arguments, &context).await
            })
            .await;
        if let Some(failure) = self.history_failure() {
            return Err(ExecutionStop::Storage(Box::new(failure.clone())).into());
        }
        let resolution = match result {
            Ok(output) => ActionResolution::Result(output),
            Err(ToolError::Feedback(output)) => ActionResolution::Failed(output),
            Err(ToolError::Failed(error)) => {
                ActionResolution::Failed(ToolOutput::text(error.to_string()))
            }
            Err(ToolError::Stop(stop)) => return Err(self.stop_dispatch_error(stop.into())),
        };
        self.resolve(handle, resolution.clone())?;
        self.check_dispatch_cancellation(execution)?;
        Ok(resolution)
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
    ) -> Result<super::PendingActionView<'_>, ToolDispatchError> {
        self.require_tool_proposal(handle)?;
        Ok(self.action(handle)?)
    }

    /// Inspect any unstarted proposal, including a submission the host may reject.
    /// This validates identity and storage state before required host writes.
    pub fn action_proposal(
        &self,
        handle: &PendingAction,
    ) -> Result<super::PendingActionView<'_>, ToolDispatchError> {
        self.require_action_proposal(handle)?;
        Ok(self.action(handle)?)
    }

    /// Stop with a typed host execution failure, retaining unfinished actions.
    /// A failed required stop write takes precedence over the supplied reason.
    /// The returned error is the authoritative stop; this never completes work.
    pub fn stop_execution(&mut self, stop: ExecutionStop) -> ToolDispatchError {
        self.stop_dispatch_error(stop.into())
    }

    pub(super) fn require_tool_proposal(
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
