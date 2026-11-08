//! The background-LLM seam for the consolidation stages.
//!
//! Prompts always arrive as a [`PromptSpec`] plus its variables, never as pre-rendered
//! strings, so callers cannot bypass strict rendering.

use std::sync::Arc;

use rig_core::completion::Message as RigMessage;

use crate::agent::harness::Harness;
use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::inference::usage::{InferenceKind, UsageContext};
use crate::tool::{AgentTool, registry::ToolFilter};
use frona_harness::inference::ModelGroup;
use tokio_util::sync::CancellationToken;

use super::prompt::{PromptSpec, RenderedPrompt};

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

    /// Render a stage's system + user prompt pair. Fails rather than returning an empty
    /// prompt - see [`PromptSpec`].
    pub fn render(&self, p: PromptSpec, vars: &[(&str, &str)]) -> Result<RenderedPrompt, AppError> {
        p.render(&self.prompts, vars)
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

    pub async fn text_with_tools(
        &self,
        agent_id: &str,
        system: &str,
        history: Vec<RigMessage>,
        filters: &[ToolFilter],
        tools: &[Arc<dyn AgentTool>],
        max_turns: usize,
    ) -> Result<String, AppError> {
        self.harness
            .text_inference_with_tools_cancel(
                agent_id,
                &self.model_group,
                system,
                history,
                filters,
                tools,
                max_turns,
                self.usage(),
                self.cancel_token.clone(),
            )
            .await
    }
}
