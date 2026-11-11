//! The background-LLM seam for the consolidation stages.
//!
//! Prompts always arrive as a [`PromptSpec`] plus its variables, never as pre-rendered
//! strings, so callers cannot bypass strict rendering.

use std::sync::Arc;

use crate::agent::harness::Harness;
use crate::core::error::AppError;
use crate::inference::usage::{InferenceKind, UsageContext};
use crate::tool::{AgentTool, registry::ToolFilter};
use frona_harness::inference::ModelGroup;
use frona_text::prompt::PromptLoader;
use tokio_util::sync::CancellationToken;

use super::prompt::PromptSpec;

mod sdk_bridge;
pub use sdk_bridge::PkmHarness;

/// Background inference for one consolidation pass, scoped to one user.
#[derive(Clone)]
pub struct ConsolidationInference {
    harness: Arc<Harness>,
    model_group: ModelGroup,
    prompts: PromptLoader,
    /// Carried so usage is attributed without reaching back into the pass scope.
    user_id: String,
    cancel_token: CancellationToken,
}

impl ConsolidationInference {
    #[cfg(test)]
    pub fn new(
        harness: Arc<Harness>,
        model_group: ModelGroup,
        prompts: PromptLoader,
        user_id: String,
    ) -> Self {
        Self::with_cancel_token(
            harness,
            model_group,
            prompts,
            user_id,
            CancellationToken::new(),
        )
    }

    pub fn with_cancel_token(
        harness: Arc<Harness>,
        model_group: ModelGroup,
        prompts: PromptLoader,
        user_id: String,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            harness,
            model_group,
            prompts,
            user_id,
            cancel_token,
        }
    }

    /// Prepare selected runtime tools once; every returned SDK run stays in memory
    /// unless its caller explicitly adds storage. No Chat/message rows are created.
    pub async fn sdk(
        &self,
        agent_id: &str,
        filters: &[ToolFilter],
        tools: &[Arc<dyn AgentTool>],
    ) -> Result<PkmHarness, AppError> {
        let tools = self
            .harness
            .prepare_agent_tools(
                &self.user_id,
                agent_id,
                None,
                filters,
                tools,
                self.cancel_token.clone(),
            )
            .await?;
        Ok(PkmHarness::new(self, tools))
    }

    pub fn prompts(&self) -> &PromptLoader {
        &self.prompts
    }

    fn usage(&self) -> UsageContext {
        UsageContext::new(
            InferenceKind::Memory,
            &self.user_id,
            self.model_group.name.clone(),
        )
    }

    /// Render a stage's correction prompt - fed back mid-conversation on a rejection.
    pub fn reject(&self, p: PromptSpec, vars: &[(&str, &str)]) -> Result<String, AppError> {
        p.reject(&self.prompts, vars)
    }

    /// Render a stage's unreadable-term correction - a CURIE that cannot be written to the
    /// schema, fed back before any reasoning is attempted.
    pub fn bad_term(&self, p: PromptSpec, vars: &[(&str, &str)]) -> Result<String, AppError> {
        p.bad_term(&self.prompts, vars)
    }

    /// Domain writes outside an SDK run must also respect caller cancellation.
    pub(crate) fn check_cancelled(&self) -> Result<(), AppError> {
        if self.cancel_token.is_cancelled() {
            return Err(AppError::Internal("PKM inference cancelled".into()));
        }
        Ok(())
    }
}
