use serde_json::Value;

use crate::core::error::AppError;
use crate::storage::resolve_workspace_attachment;
use crate::storage::service::StorageService;
use frona_derive::agent_tool;
use frona_text::prompt::PromptLoader;

use super::{InferenceContext, ToolOutput};

pub struct ProduceFileTool {
    storage: StorageService,
    prompts: PromptLoader,
}

impl ProduceFileTool {
    pub fn new(storage: StorageService, prompts: PromptLoader) -> Self {
        Self { storage, prompts }
    }
}

#[agent_tool]
impl ProduceFileTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let relative_path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'path' parameter".into()))?;

        let attachment = resolve_workspace_attachment(
            &self.storage,
            &ctx.user.handle,
            &ctx.agent.handle,
            relative_path,
        )
        .await?;

        Ok(
            ToolOutput::text(serde_json::to_string(&attachment).unwrap_or_default())
                .with_attachment(attachment),
        )
    }
}
