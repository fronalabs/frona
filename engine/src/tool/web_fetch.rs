use std::sync::Arc;

use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use frona_derive::agent_tool;

use super::{ToolContext, ToolOutput};
use super::browser::session::BrowserSessionManager;

pub struct WebFetchTool {
    session_manager: Arc<BrowserSessionManager>,
    user_id: String,
    prompts: PromptLoader,
}

impl WebFetchTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>, user_id: String, prompts: PromptLoader) -> Self {
        Self {
            session_manager,
            user_id,
            prompts,
        }
    }

    fn provider(&self) -> &str {
        "web_fetch"
    }
}

#[agent_tool]
impl WebFetchTool {
    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let url = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: url".into()))?;

        let navigate_params = serde_json::json!({
            "url": url,
            "wait_for_load": true,
        });

        self.session_manager
            .execute_tool(&self.user_id, self.provider(), "navigate", navigate_params)
            .await?;

        let markdown = self
            .session_manager
            .execute_tool(&self.user_id, self.provider(), "get_markdown", serde_json::json!({}))
            .await?;

        Ok(ToolOutput::text(markdown))
    }

    async fn cleanup(&self) -> Result<(), AppError> {
        self.session_manager
            .close_session(&self.user_id, self.provider())
            .await
    }
}
