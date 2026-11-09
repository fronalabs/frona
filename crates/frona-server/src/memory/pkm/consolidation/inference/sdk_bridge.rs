//! PKM owns prompts, identity-bound tools and usage attribution; the SDK owns the loop.
//!
//! `sdk(...).await?.run(spec, vars)?` returns an ordinary SDK Run. Callers use
//! `.structured::<T>()`, `.inspect(...)`, `.manual()`, `.budget(shared)` and
//! `.exploration_turns(n)` directly. Only Decision::accept yields an accepted
//! result. Explicit stop/abandon, inspector failure and shared-limit refusal
//! return Incomplete with any retained candidate; domain salvage is caller work.

use super::{ConsolidationInference, PromptSpec};
use crate::{
    agent::{background::PreparedAgentTools, prompt::PromptLoader},
    core::error::AppError,
    inference::usage::{UsageContext, UsageService},
    tool::{ToolOutput, sdk_adapter::content_result},
};
use frona_harness::{ExecutionStop, Run, Text};
use tokio_util::sync::CancellationToken;

/// Reusable preparation, owned so runs can safely borrow the legacy tool context.
/// It is separate from the durable PKM workflow; its conversations have no Chat ID.
pub struct PkmHarness {
    harness: frona_harness::Harness<'static>,
    tools: PreparedAgentTools,
    prompts: PromptLoader,
    usage_service: UsageService,
    usage_context: UsageContext,
    cancellation: CancellationToken,
}

impl PkmHarness {
    pub(super) fn new(inference: &ConsolidationInference, tools: PreparedAgentTools) -> Self {
        Self {
            harness: frona_harness::Harness::new(frona_harness::Model::from_group(
                inference.model_group.clone(),
            )),
            tools,
            prompts: inference.prompts.clone(),
            usage_service: inference.harness.usage_service.clone(),
            usage_context: inference.usage(),
            cancellation: inference.cancel_token.clone(),
        }
    }

    /// Strict rendering happens before constructing executable work. Tool/runtime
    /// preparation is not model execution, and failed rendering cannot consume a call.
    pub fn run(
        &self,
        prompt: PromptSpec,
        vars: &[(&str, &str)],
    ) -> Result<Run<'_, Text>, AppError> {
        self.run_with_input_suffix(prompt, vars, "")
    }

    /// Append application context, such as live-state rejection of a saved proposal,
    /// after the same strict rendering used by a fresh run.
    pub fn run_with_input_suffix(
        &self,
        prompt: PromptSpec,
        vars: &[(&str, &str)],
        suffix: &str,
    ) -> Result<Run<'_, Text>, AppError> {
        let mut rendered = prompt.render(&self.prompts, vars)?;
        rendered.input.push_str(suffix);
        let tools = self.tools.portable(map_output)?;
        Ok(self
            .harness
            .run(rendered.input)
            .instructions(rendered.system)
            .tools(tools)
            .observer(self.usage_service.scoped(&self.usage_context))
            .cancellation(self.cancellation.clone()))
    }
}

/// This bridge has no host lifecycle/event handler. Refuse unsupported host
/// effects explicitly; never turn a pending task/HITL into ordinary retry feedback.
fn map_output(output: ToolOutput) -> frona_harness::ToolResult {
    if output.hitl().is_some()
        || output.task_event().is_some()
        || output.is_pending_external()
        || output.system_prompt().is_some()
        || !output.attachments().is_empty()
    {
        return Err(ExecutionStop::Stopped(
            "PKM in-memory tools returned unsupported host effects".into(),
        )
        .into());
    }
    content_result(&output)
}

#[cfg(test)]
mod tests;
