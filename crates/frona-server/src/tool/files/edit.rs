//! No read-before-edit guard: the uniqueness check self-heals if the file
//! drifted (no-match → agent re-reads → retries with corrected old_string).

use std::sync::Arc;

use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::storage::service::StorageService;
use frona_derive::agent_tool;
use frona_text::{EditOutcome, apply_edit};

use super::super::sandbox::SandboxManager;
use super::super::{InferenceContext, ToolOutput};
use super::atomic_write;

const SNIPPET_CONTEXT_LINES: usize = 5;

pub struct EditTool {
    pub storage: StorageService,
    pub sandbox_manager: Arc<SandboxManager>,
    pub prompts: PromptLoader,
}

impl EditTool {
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
impl EditTool {
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
        let old_string = arguments
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'old_string' parameter".into()))?;
        let new_string = arguments
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'new_string' parameter".into()))?;
        let replace_all = arguments
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let resolved =
            super::resolve_path(path_arg, &ctx.user.handle, &ctx.agent.handle, &self.storage)?;
        let sandbox = self.sandbox_manager.for_tool(ctx).await?;
        if !sandbox.is_writable(&resolved) {
            return Ok(ToolOutput::error(format!(
                "Edit denied by sandbox policy: {} (resolved: {})",
                path_arg,
                resolved.display(),
            )));
        }
        if !tokio::fs::try_exists(&resolved).await.unwrap_or(false) {
            return Ok(ToolOutput::error(format!("file not found: {}", path_arg)));
        }

        let original = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| AppError::Internal(format!("read {}: {e}", resolved.display())))?;

        match apply_edit(&original, old_string, new_string, replace_all) {
            EditOutcome::EmptyNeedle => Ok(ToolOutput::error(
                "old_string must not be empty".to_string(),
            )),
            EditOutcome::NotFound => Ok(ToolOutput::error(format!(
                "old_string not found in {}. The file may have changed, or whitespace/quotes may not match.",
                path_arg
            ))),
            EditOutcome::Ambiguous { count } => Ok(ToolOutput::error(format!(
                "old_string matches {} locations in {}; provide more surrounding context to make it unique, or pass replace_all: true",
                count, path_arg,
            ))),
            EditOutcome::Applied { rewritten, count } => {
                atomic_write(&resolved, rewritten.as_bytes()).await?;
                let snippet = context_snippet(&original, &rewritten, SNIPPET_CONTEXT_LINES);
                let replacement_word = if count == 1 {
                    "replacement"
                } else {
                    "replacements"
                };
                let text = format!(
                    "Edit applied to {} ({} {}). Surrounding context:\n{}",
                    path_arg, count, replacement_word, snippet
                );
                Ok(ToolOutput::text(text))
            }
        }
    }
}

fn context_snippet(old: &str, new: &str, context: usize) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut first_change = new_lines.len();
    for (i, line) in new_lines.iter().enumerate() {
        if old_lines.get(i).copied() != Some(*line) {
            first_change = i;
            break;
        }
    }
    let start = first_change.saturating_sub(context);
    let end = (first_change + context + 1).min(new_lines.len());
    let width = (end + 1).to_string().len();
    new_lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>w$}\t{}", start + i + 1, line, w = width))
        .collect::<Vec<_>>()
        .join("\n")
}
