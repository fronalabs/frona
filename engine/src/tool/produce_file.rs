use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use crate::api::files::{detect_content_type, make_agent_path};
use crate::error::AppError;

use super::{AgentTool, ToolContext, ToolDefinition, ToolOutput};

pub struct ProduceFileTool {
    agent_id: String,
    workspace_path: PathBuf,
}

impl ProduceFileTool {
    pub fn new(agent_id: String, workspace_path: PathBuf) -> Self {
        Self {
            agent_id,
            workspace_path,
        }
    }
}

#[async_trait]
impl AgentTool for ProduceFileTool {
    fn name(&self) -> &str {
        "produce_file"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "produce_file".to_string(),
            description: "Register a file from your workspace as a produced output. \
                The file must already exist in your workspace. \
                This makes the file available for the user to download \
                and propagates it to parent agents when completing delegated tasks."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to your workspace (e.g. output.csv or subdir/report.pdf)"
                    }
                },
                "required": ["path"]
            }),
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let relative_path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'path' parameter".into()))?;

        if relative_path.contains("..") {
            return Err(AppError::Validation(
                "Path traversal not allowed".into(),
            ));
        }

        let resolved = self.workspace_path.join(relative_path);

        if !resolved.exists() {
            return Err(AppError::NotFound(format!(
                "File not found in workspace: {relative_path}"
            )));
        }

        let metadata = tokio::fs::metadata(&resolved)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read file metadata: {e}")))?;

        let filename = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(relative_path)
            .to_string();

        let content_type = detect_content_type(&filename).to_string();
        let virtual_path = make_agent_path(&self.agent_id, relative_path);

        let attachment = crate::api::files::Attachment {
            filename: filename.clone(),
            content_type: content_type.clone(),
            size_bytes: metadata.len(),
            path: virtual_path.clone(),
        };

        Ok(ToolOutput::text(serde_json::to_string(&attachment).unwrap_or_default())
            .with_attachment(attachment))
    }
}
