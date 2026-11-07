//! Individual action dispatch shared by future manual and automatic drivers.

use super::{
    ActionKind, ActionResolution, ActionStatus, Conversation, ConversationError, ExecutionControl,
    ExecutionStop, PendingAction, ToolContext, ToolError, ToolOutput, ToolRegistry,
};

#[derive(Debug, thiserror::Error)]
pub enum ToolDispatchError {
    #[error(transparent)]
    Conversation(ConversationError),
    #[error("submission actions require submission resolution, not tool dispatch")]
    Submission,
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
    /// Execute one proposed ordinary tool. Validation happens before admission
    /// or effects; each call gets its own budget permit. Refused/unknown tools
    /// receive paired feedback without invocation or consumption. Typed execution
    /// stops terminate this conversation and leave unfinished actions intact.
    pub async fn execute_tool(
        &mut self,
        handle: &PendingAction,
        tools: &mut ToolRegistry<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<ActionResolution, ToolDispatchError> {
        self.require_tool_proposal(handle)?;
        if let Err(error) = execution.check_cancellation() {
            return Err(self.stop_dispatch(
                error
                    .into_execution_stop()
                    .expect("cancellation is terminal"),
            ));
        }
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
        let execution = execution.with_action(self.action_position(handle)?);
        let context = ToolContext::for_execution(call.id.as_str().into(), execution);
        let result = execution
            .with_cancellation(&context.cancellation)
            .tool_attempt(&name, || async {
                // The exclusive conversation borrow has remained held since
                // validation; no other transition can invalidate this proposal.
                self.start(handle).map_err(|error| match error {
                    ConversationError::Storage(failure) => {
                        ToolError::Stop(ExecutionStop::Storage(Box::new((*failure).clone())))
                    }
                    error => ToolError::failed(error),
                })?;
                execution.check_cancellation()?;
                tool.call(call.function.arguments, &context).await
            })
            .await;
        // Cancellation may arrive from the required write or budget callback.
        // Preserve its failed-write diagnostic rather than replacing it with a
        // secondary cancellation stop.
        if let Some(failure) = self.history_failure() {
            return Err(ExecutionStop::Storage(Box::new(failure.clone())).into());
        }
        let resolution = match result {
            Ok(output) => ActionResolution::Result(output),
            Err(ToolError::Failed(error)) => {
                ActionResolution::Failed(ToolOutput::text(error.to_string()))
            }
            Err(ToolError::Stop(stop)) => {
                return Err(self.stop_dispatch(stop));
            }
        };
        self.resolve(handle, resolution.clone())?;
        Ok(resolution)
    }

    fn stop_dispatch(&mut self, stop: ExecutionStop) -> ToolDispatchError {
        match self.stop_with_reason(stop.failure_kind(), stop.to_string()) {
            Err(error) => error.into(),
            Ok(()) => stop.into(),
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

    pub(super) fn require_tool_proposal(
        &self,
        handle: &PendingAction,
    ) -> Result<(), ToolDispatchError> {
        let view = self.action(handle)?;
        if view.status == ActionStatus::Started {
            return Err(ConversationError::AlreadyStarted.into());
        }
        if view.kind != ActionKind::Tool {
            return Err(ToolDispatchError::Submission);
        }
        Ok(())
    }
}
