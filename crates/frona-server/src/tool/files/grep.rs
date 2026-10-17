use std::sync::Arc;

use serde_json::Value;
use tokio::io::AsyncBufReadExt;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::storage::service::StorageService;
use frona_derive::agent_tool;

use super::super::sandbox::SandboxManager;
use super::super::{InferenceContext, ToolOutput};
use frona_text::walk_with_ignore;

const MAX_MATCHES: usize = 1000;
const MAX_LINE_CHARS: usize = 500;

pub struct GrepTool {
    pub storage: StorageService,
    pub sandbox_manager: Arc<SandboxManager>,
    pub prompts: PromptLoader,
}

impl GrepTool {
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
impl GrepTool {
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
                "Grep denied by sandbox policy: {} (resolved: {})",
                scope_input,
                resolved.display(),
            )));
        }

        let re = regex::Regex::new(pattern)
            .map_err(|e| AppError::Validation(format!("invalid regex: {e}")))?;

        let mut results: Vec<String> = Vec::new();
        let mut truncated = false;
        let files: Vec<std::path::PathBuf> = if resolved.is_file() {
            vec![resolved.clone()]
        } else if resolved.is_dir() {
            walk_with_ignore(&resolved).collect()
        } else {
            return Ok(ToolOutput::error(format!(
                "scope path not found: {}",
                scope_input
            )));
        };

        let scope_root = if resolved.is_file() {
            resolved
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| resolved.clone())
        } else {
            resolved.clone()
        };

        for file in files {
            if truncated {
                break;
            }
            let Ok(f) = tokio::fs::File::open(&file).await else {
                continue;
            };
            let reader = tokio::io::BufReader::new(f);
            let mut lines = reader.lines();
            let mut line_no: usize = 0;
            while let Some(line) = lines.next_line().await.ok().flatten() {
                line_no += 1;
                if !re.is_match(&line) {
                    continue;
                }
                let trimmed: String = line.chars().take(MAX_LINE_CHARS).collect();
                let suffix = if line.chars().count() > MAX_LINE_CHARS {
                    "…"
                } else {
                    ""
                };
                let rel = file.strip_prefix(&scope_root).unwrap_or(&file);
                results.push(format!(
                    "{}:{}:{}{}",
                    rel.display(),
                    line_no,
                    trimmed,
                    suffix
                ));
                if results.len() >= MAX_MATCHES {
                    truncated = true;
                    break;
                }
            }
        }

        let mut body = if results.is_empty() {
            format!("No matches for /{}/ under {}.", pattern, scope_input)
        } else {
            results.join("\n")
        };
        if truncated {
            body.push_str(&format!(
                "\n\n[truncated at {} matches; narrow your pattern]",
                MAX_MATCHES
            ));
        }
        Ok(ToolOutput::text(body))
    }
}
