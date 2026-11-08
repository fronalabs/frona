//! Explicit preparation shared by automatic and application-driven continuation.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResumeError {
    #[error("call recover().await before using a resumed manual session")]
    PreparationRequired,
    #[error("restored output mode/schema differs from the configured run")]
    ModeMismatch,
    #[error("resumed instructions must match the saved history")]
    InstructionsChanged,
}

impl<Mode> ManualSession<'_, Mode> {
    /// Prepare a session created by Harness::resume and retry interrupted tools.
    /// Once prepared, this returns pending handles without another retry. For a
    /// later interruption, hand into_conversation to Harness::resume for a fresh
    /// execution/report scope. Unstarted proposals and submissions remain
    /// caller-controlled. No model is invoked.
    pub async fn recover(&mut self) -> Result<ResponseActions, ManualError> {
        if self.core.resume_pending {
            if let Err(error) = self.core.prepare_resume().await {
                return Err(self.core.fail(error));
            }
        } else {
            self.core.check_active()?;
        }
        self.pending_actions()
    }

    /// Hand ownership back to the caller. Deferred file intent and any failed
    /// write remain attached; this does not implicitly switch to memory storage.
    pub fn into_conversation(self) -> Conversation {
        self.core.conversation
    }

    /// Save application progress even at an incomplete or completed boundary.
    /// Resumed sessions must prepare first; loading a checkpoint needs no recovery.
    pub fn save_checkpoint<T: serde::Serialize + ?Sized>(
        &mut self,
        schema: impl Into<String>,
        version: u64,
        payload: &T,
    ) -> Result<u64, ManualError> {
        if self.core.resume_pending {
            return Err(ResumeError::PreparationRequired.into());
        }
        if let Some(error) = self.core.setup_error.take() {
            return Err(self.core.fail(error));
        }
        self.core
            .conversation
            .configure_history(&self.core.instructions, self.core.submission.as_ref());
        let result = self
            .core
            .conversation
            .save_checkpoint(schema, version, payload);
        self.core.result(result)
    }

    pub fn load_checkpoint<T: DeserializeOwned>(
        &self,
        schema: &str,
        version: u64,
    ) -> Result<Option<super::super::LoadedCheckpoint<T>>, super::super::CheckpointLoadError> {
        self.core.conversation.load_checkpoint(schema, version)
    }

    pub(crate) async fn next_response(&mut self) -> Result<ResponseActions, ManualError> {
        self.recover().await?;
        if std::mem::take(&mut self.core.resume_response) {
            return self.pending_actions();
        }
        self.call_model().await
    }
}

impl SessionCore<'_> {
    fn validate_resume(&self) -> Result<(), ManualError> {
        if let Some(failure) = self.conversation.history_failure() {
            return Err(ExecutionStop::Storage(Box::new(failure.clone())).into());
        }
        if self.conversation.pending_write().is_some() {
            return Err(ConversationError::PendingWrite.into());
        }
        if self.conversation.status() == super::super::ConversationStatus::Finished {
            return Err(
                ToolDispatchError::Recovery(super::super::RecoveryError::AlreadyCompleted).into(),
            );
        }
        if self.recorder.0.snapshot().outcome.is_some() {
            return Err(
                ToolDispatchError::Recovery(super::super::RecoveryError::FinalizedReport).into(),
            );
        }
        if let Some((instructions, mode)) = self.conversation.saved_configuration() {
            if instructions != self.instructions {
                return Err(ResumeError::InstructionsChanged.into());
            }
            let configured = self
                .submission
                .as_ref()
                .map(|tool| super::super::HistoryMode::Structured(tool.parameters.clone()))
                .unwrap_or(super::super::HistoryMode::Text);
            if *mode != configured {
                return Err(ResumeError::ModeMismatch.into());
            }
        }
        let positions = self.conversation.model_view()?.original_positions();
        self.check_compatibility(&self.model, &positions)?;
        let request = self.model_request(ToolSelection::All);
        self.model.validate_request(
            &request,
            Self::streams_tokens(&self.model, &self.progress.publisher(), true),
        )?;
        Ok(())
    }

    async fn prepare_resume(&mut self) -> Result<(), ManualError> {
        self.validate_resume()?;
        if let Some(error) = self.setup_error.take() {
            return Err(error);
        }
        if self.cancellation.is_cancelled() {
            return Err(
                ExecutionStop::Cancelled("execution cancelled before recovery".into()).into(),
            );
        }
        #[cfg(feature = "jsonl")]
        self.conversation.prepare_jsonl()?;
        self.conversation
            .configure_history(&self.instructions, self.submission.as_ref());
        let execution = ExecutionControl::new(&*self.budget)
            .with_cancellation(&self.cancellation)
            .with_report(&self.recorder.0);
        let result = match &mut self.tool_hook {
            Some(hook) => {
                self.conversation
                    .recover_tools_with_hook(&mut self.tools, execution, &mut **hook)
                    .await
            }
            None => {
                self.conversation
                    .recover_tools(&mut self.tools, execution)
                    .await
            }
        };
        match result {
            Ok(_) => {}
            Err(ToolDispatchError::Stop(stop)) => return Err(stop.into()),
            Err(error) => return Err(error.into()),
        }
        self.resume_response = self.conversation.pending_count() != 0
            || self
                .conversation
                .responses()
                .last()
                .is_some_and(|response| {
                    let message_count = self.conversation.messages().len();
                    let untouched_response =
                        response.message_position().checked_add(1) == Some(message_count);
                    // Typed continuation must still run the shared submission
                    // policy after a completed action batch. Its derived result
                    // message alone does not mean missing-submit repair ran.
                    // Later user feedback moves the tail beyond this boundary.
                    let completed_batch = self.submission.is_some()
                        && !response.actions().is_empty()
                        && response.message_position().checked_add(2) == Some(message_count);
                    untouched_response || completed_batch
                });
        self.resume_pending = false;
        Ok(())
    }
}
