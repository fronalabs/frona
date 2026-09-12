//! The background-LLM seam for the consolidation stages.
//!
//! Prompts always arrive as a [`PromptSpec`] plus its variables, never as pre-rendered
//! strings, so callers cannot bypass strict rendering.

use std::sync::Arc;

use rig_core::completion::Message as RigMessage;

use crate::agent::harness::Harness;
use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::inference::ModelGroup;
use crate::inference::usage::{InferenceKind, UsageContext};
use crate::inference::{AnswerAttempt, StructuredConversation};
use crate::tool::AgentTool;
use crate::tool::registry::ToolFilter;
use tokio_util::sync::CancellationToken;

use super::prompt::{PromptSpec, RenderedPrompt};

/// What domain validation decided about one valid structured submission.
pub(crate) enum Verdict<A> {
    Accept(A),
    Stop(A),
    Abandon,
    Revise { feedback: String, keep: Option<A> },
}

/// A consolidation dialogue that owns submission accounting and domain refinement.
pub(crate) struct ConsolidationConversation<'a, T> {
    inner: StructuredConversation<'a, T>,
}

impl<'a, T> ConsolidationConversation<'a, T>
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
{
    pub async fn refine<A, Fut>(
        &mut self,
        max_submissions: usize,
        mut inspect: impl FnMut(T) -> Fut,
    ) -> Result<Option<A>, AppError>
    where
        Fut: std::future::Future<Output = Result<Verdict<A>, AppError>> + Send,
    {
        if max_submissions == 0 {
            return Err(AppError::Internal(
                "conversation: max_submissions must be at least one".into(),
            ));
        }

        let mut kept: Option<A> = None;
        let mut produced_candidate = false;
        for _ in 0..max_submissions {
            let attempt = match self.inner.next_attempt().await {
                Ok(attempt) => attempt,
                Err(e) if !produced_candidate => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "structured conversation: revision did not converge");
                    break;
                }
            };
            let AnswerAttempt::Submitted(candidate) = attempt else {
                continue;
            };
            produced_candidate = true;
            match inspect(candidate).await? {
                Verdict::Accept(value) | Verdict::Stop(value) => return Ok(Some(value)),
                Verdict::Abandon => break,
                Verdict::Revise { feedback, keep } => {
                    if keep.is_some() {
                        kept = keep;
                    }
                    self.inner.reject_submission(feedback)?;
                }
            }
        }

        if produced_candidate || kept.is_some() {
            Ok(kept)
        } else {
            Err(AppError::Internal(
                "conversation: submission budget exhausted without a valid submission".into(),
            ))
        }
    }

    pub fn requests_used(&self) -> usize {
        self.inner.requests_used()
    }
}

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

    /// A multi-turn tool conversation: the model proposes, the system validates
    /// externally, and rejections are fed back for a revision.
    /// The returned conversation borrows the harness for its lifetime, so it must not
    /// outlive this `ConsolidationInference`.
    #[allow(clippy::too_many_arguments)]
    pub async fn conversation<'a, T>(
        &'a self,
        chat_id: Option<&str>,
        agent_id: &str,
        system: String,
        initial: String,
        filters: &[ToolFilter],
        tools: &[Arc<dyn AgentTool>],
        max_tool_turns: usize,
    ) -> Result<ConsolidationConversation<'a, T>, AppError>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        let inner = self
            .harness
            .structured_conversation_with_cancel::<T>(
                chat_id,
                agent_id,
                &self.model_group,
                system,
                initial,
                filters,
                tools,
                max_tool_turns,
                self.usage(),
                self.cancel_token.clone(),
            )
            .await?;
        Ok(ConsolidationConversation { inner })
    }
}
