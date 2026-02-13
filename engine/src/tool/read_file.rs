use std::path::PathBuf;

use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::config::Config;
use crate::api::files::{
    detect_content_type, is_image_content_type, is_text_content_type, resolve_virtual_path,
};
use crate::core::error::AppError;
use frona_derive::agent_tool;

use super::{ImageData, ToolContext, ToolOutput};

pub struct ReadFileTool {
    config: Config,
    prompts: PromptLoader,
}

impl ReadFileTool {
    pub fn new(config: Config, prompts: PromptLoader) -> Self {
        Self { config, prompts }
    }
}

#[agent_tool]
impl ReadFileTool {
    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'path' parameter".into()))?;

        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500) as usize;

        let resolved = resolve_virtual_path(path, &self.config)?;

        if !resolved.exists() {
            return Err(AppError::NotFound(format!("File not found: {path}")));
        }

        let filename = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let content_type = detect_content_type(filename);

        if is_image_content_type(content_type) {
            read_image(&resolved, filename, content_type).await
        } else if is_text_content_type(content_type) {
            read_text(&resolved, filename, offset, limit).await
        } else {
            read_binary_metadata(&resolved, filename).await
        }
    }
}

async fn read_text(
    path: &PathBuf,
    filename: &str,
    offset: usize,
    limit: usize,
) -> Result<ToolOutput, AppError> {
    let content =
        tokio::fs::read_to_string(path)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read file: {e}")))?;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = offset.min(total);
    let end = (start + limit).min(total);
    let selected: String = lines[start..end].join("\n");

    let header = if offset > 0 || end < total {
        format!("[{filename} lines {}-{} of {total}]\n", start + 1, end)
    } else {
        format!("[{filename}]\n")
    };

    Ok(ToolOutput::text(format!("{header}{selected}")))
}

async fn read_image(
    path: &PathBuf,
    filename: &str,
    content_type: &str,
) -> Result<ToolOutput, AppError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to read image: {e}")))?;

    Ok(ToolOutput::mixed(
        format!("Image: {filename}"),
        vec![ImageData {
            bytes,
            media_type: content_type.to_string(),
        }],
    ))
}

async fn read_binary_metadata(path: &PathBuf, filename: &str) -> Result<ToolOutput, AppError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to read file metadata: {e}")))?;

    Ok(ToolOutput::text(format!(
        "Binary file: {filename} ({} bytes)",
        metadata.len()
    )))
}
