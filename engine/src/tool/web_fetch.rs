use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::AppError;
use crate::tool::{AgentTool, ToolContext, ToolDefinition, ToolOutput};

use super::browser::session::BrowserSessionManager;

pub struct WebFetchTool {
    session_manager: Arc<BrowserSessionManager>,
    user_id: String,
}

impl WebFetchTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>, user_id: String) -> Self {
        Self {
            session_manager,
            user_id,
        }
    }

    fn provider(&self) -> &str {
        "web_fetch"
    }
}

#[async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a web page and return its content as markdown.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL of the web page to fetch"
                    }
                },
                "required": ["url"]
            }),
        }]
    }

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
