//! `write` — create a new file, or overwrite an existing one with the
//! `overwrite: true` opt-in flag.
//!
//! Create-only-by-default is the structural guard against the "agent
//! overwrites without realising" bug. The escape hatch is `overwrite: true`.

use std::sync::Arc;

use serde_json::Value;

use crate::core::error::AppError;
use crate::storage::service::StorageService;
use frona_derive::agent_tool;
use frona_text::prompt::PromptLoader;

use super::super::sandbox::SandboxManager;
use super::super::{InferenceContext, ToolOutput};
use super::atomic_write;

pub struct WriteTool {
    pub storage: StorageService,
    pub sandbox_manager: Arc<SandboxManager>,
    pub prompts: PromptLoader,
}

impl WriteTool {
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
impl WriteTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let path_arg = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'path' parameter".into()))?;
        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'content' parameter".into()))?;
        let overwrite = arguments
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let resolved =
            super::resolve_path(path_arg, &ctx.user.handle, &ctx.agent.handle, &self.storage)?;
        let sandbox = self.sandbox_manager.for_tool(ctx).await?;
        if !sandbox.is_writable(&resolved) {
            return Ok(ToolOutput::error(format!(
                "Write denied by sandbox policy: {} (resolved: {})",
                path_arg,
                resolved.display(),
            )));
        }

        let exists = tokio::fs::try_exists(&resolved).await.unwrap_or(false);
        if exists && !overwrite {
            return Ok(ToolOutput::error(format!(
                "file already exists at {}; pass overwrite: true to replace it, or use edit to modify it",
                path_arg
            )));
        }

        atomic_write(&resolved, content.as_bytes()).await?;

        Ok(ToolOutput::text(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path_arg
        )))
    }
}
