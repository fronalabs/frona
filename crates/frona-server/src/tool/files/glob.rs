use std::sync::Arc;

use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::storage::service::StorageService;
use frona_derive::agent_tool;

use super::super::sandbox::SandboxManager;
use super::super::{InferenceContext, ToolOutput};

pub struct GlobTool {
    pub storage: StorageService,
    pub sandbox_manager: Arc<SandboxManager>,
    pub prompts: PromptLoader,
}

impl GlobTool {
    pub fn new(
        storage: StorageService,
        sandbox_manager: Arc<SandboxManager>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            storage,
            sandbox_manager,
            prompts,
        }
    }
}

#[agent_tool]
impl GlobTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let pattern = arguments
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'pattern' parameter".into()))?;
        let scope_arg = arguments.get("path").and_then(|v| v.as_str());

        let scope_input = scope_arg.unwrap_or(".");
        let resolved = super::resolve_path(
            scope_input,
            &ctx.user.handle,
            &ctx.agent.handle,
            &self.storage,
        )?;
        let sandbox = self.sandbox_manager.for_tool(ctx).await?;
        if !sandbox.is_readable(&resolved) {
            return Ok(ToolOutput::error(format!(
                "Glob denied by sandbox policy: {} (resolved: {})",
                scope_input,
                resolved.display(),
            )));
        }
        frona_harness::tools::files::glob::glob(&resolved, scope_input, pattern)
            .map(super::file_output)
            .map_err(Into::into)
    }
}
