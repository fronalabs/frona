use std::sync::Arc;

use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::storage::service::StorageService;
use frona_derive::agent_tool;

use super::super::sandbox::SandboxManager;
use super::super::{InferenceContext, ToolOutput};
use frona_text::walk_with_ignore;

const MAX_RESULTS: usize = 1000;

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
        if !resolved.is_dir() {
            return Ok(ToolOutput::error(format!(
                "glob scope must be a directory; got file {}",
                scope_input
            )));
        }

        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map_err(|e| AppError::Validation(format!("invalid glob pattern: {e}")))?
            .compile_matcher();

        let mut results = Vec::new();
        let mut truncated = false;
        for path in walk_with_ignore(&resolved) {
            let rel = path.strip_prefix(&resolved).unwrap_or(&path);
            if matcher.is_match(rel) {
                results.push(rel.display().to_string());
                if results.len() >= MAX_RESULTS {
                    truncated = true;
                    break;
                }
            }
        }

        let mut body = if results.is_empty() {
            format!("No matches for pattern {} under {}.", pattern, scope_input)
        } else {
            results.join("\n")
        };
        if truncated {
            body.push_str(&format!(
                "\n\n[truncated at {} matches; narrow your pattern]",
                MAX_RESULTS
            ));
        }
        Ok(ToolOutput::text(body))
    }
}
