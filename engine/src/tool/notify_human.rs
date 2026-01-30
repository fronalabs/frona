use async_trait::async_trait;
use serde_json::Value;

use crate::error::AppError;

use super::{AgentTool, ToolDefinition, ToolOutput, ToolType};

const EXTERNAL_TOOLS: &[&str] = &["ask_human_question", "request_human_takeover"];

pub struct NotifyHumanTool {
    debugger_url: Option<String>,
}

impl NotifyHumanTool {
    pub fn new(credential_id: Option<String>) -> Self {
        let debugger_url =
            credential_id.map(|id| format!("/api/browser/debugger/{id}"));
        Self { debugger_url }
    }
}

#[async_trait]
impl AgentTool for NotifyHumanTool {
    fn name(&self) -> &str {
        "notify_human"
    }

    fn tool_type(&self, tool_name: &str) -> ToolType {
        if EXTERNAL_TOOLS.contains(&tool_name) {
            ToolType::External
        } else {
            ToolType::Internal
        }
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "request_human_takeover".to_string(),
                description: "Request the human to take over the browser session (e.g. for CAPTCHA, 2FA, login). The debugger URL is automatically generated from the last browser profile used. Creates a notification and returns immediately.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "reason": {
                            "type": "string",
                            "description": "Why human intervention is needed"
                        }
                    },
                    "required": ["reason"]
                }),
            },
            ToolDefinition {
                name: "ask_human_question".to_string(),
                description: "Ask the human a question and wait for their response. Creates a notification and returns immediately.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask"
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Available answer options"
                        }
                    },
                    "required": ["question", "options"]
                }),
            },
            ToolDefinition {
                name: "warn_human".to_string(),
                description: "Send a warning notification to the human. Non-blocking.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "Warning message"
                        }
                    },
                    "required": ["message"]
                }),
            },
            ToolDefinition {
                name: "inform_human".to_string(),
                description: "Send an informational notification to the human. Non-blocking.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "Info message"
                        }
                    },
                    "required": ["message"]
                }),
            },
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Value) -> Result<ToolOutput, AppError> {
        match tool_name {
            "request_human_takeover" => {
                let reason = arguments
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Human intervention needed")
                    .to_string();
                let debugger_url = self.debugger_url.clone().unwrap_or_default();

                Ok(ToolOutput::text(serde_json::json!({
                    "tool_type": "HumanInTheLoop",
                    "reason": reason,
                    "debugger_url": debugger_url,
                }).to_string()))
            }
            "ask_human_question" => {
                let question = arguments
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let options: Vec<String> = arguments
                    .get("options")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                Ok(ToolOutput::text(serde_json::json!({
                    "tool_type": "Question",
                    "question": question,
                    "options": options,
                }).to_string()))
            }
            "warn_human" => {
                let message = arguments
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Warning")
                    .to_string();

                Ok(ToolOutput::text(serde_json::json!({
                    "tool_type": "Warning",
                    "message": message,
                }).to_string()))
            }
            "inform_human" => {
                let message = arguments
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Info")
                    .to_string();

                Ok(ToolOutput::text(serde_json::json!({
                    "tool_type": "Info",
                    "message": message,
                }).to_string()))
            }
            _ => Err(AppError::Tool(format!(
                "Unknown notify_human sub-tool: {tool_name}"
            ))),
        }
    }
}
