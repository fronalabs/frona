use std::sync::Arc;

use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::storage::service::StorageService;
use frona_derive::agent_tool;

use super::super::image::{is_supported_image, prepare_image};
use super::super::sandbox::SandboxManager;
use super::super::{InferenceContext, ToolOutput};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

pub struct ReadTool {
    pub storage: StorageService,
    pub sandbox_manager: Arc<SandboxManager>,
    pub prompts: PromptLoader,
}

impl ReadTool {
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
impl ReadTool {
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
        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        let resolved =
            super::resolve_path(path_arg, &ctx.user.handle, &ctx.agent.handle, &self.storage)?;
        let sandbox = self.sandbox_manager.for_tool(ctx).await?;
        if !sandbox.is_readable(&resolved) {
            return Ok(ToolOutput::error(format!(
                "Read denied by sandbox policy: {} (resolved: {})",
                path_arg,
                resolved.display(),
            )));
        }
        if !tokio::fs::try_exists(&resolved).await.unwrap_or(false) {
            return Ok(ToolOutput::error(format!("file not found: {}", path_arg)));
        }

        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|e| AppError::Internal(format!("read {}: {e}", resolved.display())))?;
        let size = bytes.len();

        let mime = infer::get(&bytes).map(|t| t.mime_type().to_string());
        if let Some(ref m) = mime
            && is_supported_image(m)
        {
            return read_image(&bytes, m, path_arg);
        }

        match std::str::from_utf8(&bytes) {
            Ok(text) => Ok(read_text(text, offset, limit)),
            Err(_) => Ok(ToolOutput::error(format!(
                "Read got a binary file ({}, {} bytes) at {}. Use produce_file to surface it to the user, or pass through CliTool if you need raw bytes.",
                mime.as_deref().unwrap_or("application/octet-stream"),
                size,
                path_arg,
            ))),
        }
    }
}

fn read_image(bytes: &[u8], mime: &str, path_arg: &str) -> Result<ToolOutput, AppError> {
    Ok(match prepare_image(bytes, mime) {
        Ok(image) => ToolOutput::mixed(format!("Read image file [{mime}]"), vec![image]),
        Err(reason) => ToolOutput::error(format!("{reason} at {path_arg} ({mime})")),
    })
}

fn read_text(text: &str, offset: Option<usize>, limit: Option<usize>) -> ToolOutput {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let start = offset.map(|n| n.saturating_sub(1)).unwrap_or(0);
    if start >= total_lines && total_lines > 0 {
        return ToolOutput::error(format!(
            "offset {} is past end of file ({} lines)",
            start + 1,
            total_lines
        ));
    }

    let end_unlimited = match limit {
        Some(l) => (start + l).min(total_lines),
        None => total_lines,
    };

    let mut byte_count = 0usize;
    let mut last_line = start;
    for (i, line) in lines[start..end_unlimited].iter().enumerate() {
        let next = byte_count + line.len() + 1; // +1 for the trailing newline
        if i >= MAX_LINES || next > MAX_BYTES {
            break;
        }
        byte_count = next;
        last_line = start + i + 1;
    }

    let mut body = lines[start..last_line].join("\n");
    let truncated_by_cap = last_line < end_unlimited;
    let more_after_limit = end_unlimited < total_lines;

    if truncated_by_cap || more_after_limit {
        let shown_first = start + 1;
        let shown_last = last_line.max(start + 1);
        let next_offset = last_line + 1;
        body.push_str(&format!(
            "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
            shown_first, shown_last, total_lines, next_offset
        ));
    }

    if body.is_empty() {
        ToolOutput::text(String::new())
    } else {
        ToolOutput::text(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_text_no_truncation() {
        let out = read_text("hello\nworld", None, None);
        assert_eq!(out.text_content(), "hello\nworld");
    }

    #[test]
    fn read_text_offset_limit() {
        let text = "a\nb\nc\nd\ne";
        let out = read_text(text, Some(2), Some(2));
        // start at line 2 ("b"), 2 lines → "b\nc"
        assert!(out.text_content().contains("b\nc"));
        // more remain, so continuation hint
        assert!(out.text_content().contains("Use offset=4"));
    }

    #[test]
    fn read_text_offset_past_end() {
        let out = read_text("a\nb", Some(99), None);
        assert!(!out.is_success());
    }

    #[test]
    fn is_image_branch() {
        assert!(is_supported_image("image/png"));
        assert!(is_supported_image("image/jpeg"));
        assert!(!is_supported_image("application/pdf"));
    }
}
