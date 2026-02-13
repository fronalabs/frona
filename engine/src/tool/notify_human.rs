use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;

use crate::chat::message::models::{MessageTool, ToolStatus};
use frona_derive::agent_tool;

use super::{ToolContext, ToolOutput, ToolType};

const EXTERNAL_TOOLS: &[&str] = &["ask_user_question", "request_user_takeover"];

pub struct NotifyHumanTool {
    debugger_url: Option<String>,
    prompts: PromptLoader,
}

impl NotifyHumanTool {
    pub fn new(credential_id: Option<String>, prompts: PromptLoader) -> Self {
        let debugger_url =
            credential_id.map(|id| format!("/api/browser/debugger/{id}"));
        Self { debugger_url, prompts }
    }
}

#[agent_tool(files("request_user_takeover", "ask_user_question"))]
impl NotifyHumanTool {
    fn tool_type(&self, tool_name: &str) -> ToolType {
        if EXTERNAL_TOOLS.contains(&tool_name) {
            ToolType::External
        } else {
            ToolType::Internal
        }
    }

    async fn execute(&self, tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        match tool_name {
            "request_user_takeover" => {
                let reason = arguments
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("User intervention needed")
                    .to_string();
                let debugger_url = self.debugger_url.clone().unwrap_or_default();

                let json = serde_json::json!({
                    "tool_type": "HumanInTheLoop",
                    "reason": reason,
                    "debugger_url": debugger_url,
                });

                Ok(ToolOutput::text(json.to_string())
                    .with_tool_data(MessageTool::HumanInTheLoop {
                        reason,
                        debugger_url,
                        status: ToolStatus::Pending,
                        response: None,
                    }))
            }
            "ask_user_question" => {
                let question = arguments
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let options: Vec<String> = arguments
                    .get("options")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                let json = serde_json::json!({
                    "tool_type": "Question",
                    "question": question,
                    "options": options,
                });

                Ok(ToolOutput::text(json.to_string())
                    .with_tool_data(MessageTool::Question {
                        question,
                        options,
                        status: ToolStatus::Pending,
                        response: None,
                    }))
            }
            _ => Err(AppError::Tool(format!(
                "Unknown notify_human sub-tool: {tool_name}"
            ))),
        }
    }
}
