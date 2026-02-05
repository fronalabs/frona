use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::AppError;
use crate::tool::{AgentTool, ImageData, ToolContext, ToolDefinition, ToolOutput};

use super::session::BrowserSessionManager;

pub struct BrowserTool {
    session_manager: Arc<BrowserSessionManager>,
    user_id: String,
    provider: String,
}

impl BrowserTool {
    pub fn new(
        session_manager: Arc<BrowserSessionManager>,
        user_id: String,
        provider: String,
    ) -> Self {
        Self {
            session_manager,
            user_id,
            provider,
        }
    }
}

#[async_trait]
impl AgentTool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            // --- Navigation ---
            ToolDefinition {
                name: "browser_navigate".to_string(),
                description: "Navigate to a URL in the browser.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "URL to navigate to"
                        },
                        "wait_for_load": {
                            "type": "boolean",
                            "description": "Wait for navigation to complete",
                            "default": true
                        }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "browser_go_back".to_string(),
                description: "Navigate back in browser history.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "browser_go_forward".to_string(),
                description: "Navigate forward in browser history.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "browser_close".to_string(),
                description: "Close the browser when the task is complete.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            // --- Content extraction ---
            ToolDefinition {
                name: "browser_extract".to_string(),
                description: "Extract text or HTML content from the current page or a specific element.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "string",
                            "description": "CSS selector to extract from (defaults to body)"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["text", "html"],
                            "description": "Output format",
                            "default": "text"
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_read_links".to_string(),
                description: "Get all links on the current page with their text and URLs.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "browser_get_markdown".to_string(),
                description: "Get the markdown content of the current page with pagination support.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "page": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Page number to extract (1-based)",
                            "default": 1
                        },
                        "page_size": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Maximum characters per page",
                            "default": 100000
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_snapshot".to_string(),
                description: "Get an ARIA snapshot of the page with indexed interactive elements.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "incremental": {
                            "type": "boolean",
                            "description": "Return incremental snapshot instead of full",
                            "default": false
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_screenshot".to_string(),
                description: "Capture a screenshot of the current page.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to save the screenshot"
                        },
                        "full_page": {
                            "type": "boolean",
                            "description": "Capture full page instead of viewport",
                            "default": false
                        }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "browser_evaluate".to_string(),
                description: "Execute JavaScript code in the browser context.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "JavaScript code to execute"
                        },
                        "await_promise": {
                            "type": "boolean",
                            "description": "Wait for promise resolution",
                            "default": false
                        }
                    },
                    "required": ["code"]
                }),
            },
            // --- Interaction ---
            ToolDefinition {
                name: "browser_click".to_string(),
                description: "Click on a DOM element by CSS selector or snapshot index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "string",
                            "description": "CSS selector of the element to click"
                        },
                        "index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Snapshot index of the element to click"
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_hover".to_string(),
                description: "Hover over a DOM element by CSS selector or snapshot index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "string",
                            "description": "CSS selector of the element to hover"
                        },
                        "index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Snapshot index of the element to hover"
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_select".to_string(),
                description: "Select an option in a dropdown element.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "string",
                            "description": "CSS selector of the dropdown"
                        },
                        "index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Snapshot index of the dropdown"
                        },
                        "value": {
                            "type": "string",
                            "description": "Value to select in the dropdown"
                        }
                    },
                    "required": ["value"]
                }),
            },
            ToolDefinition {
                name: "browser_input_fill".to_string(),
                description: "Type text into an input element by CSS selector or snapshot index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "string",
                            "description": "CSS selector of the input element"
                        },
                        "index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Snapshot index of the input element"
                        },
                        "text": {
                            "type": "string",
                            "description": "Text to type into the element"
                        },
                        "clear": {
                            "type": "boolean",
                            "description": "Clear existing content first",
                            "default": false
                        }
                    },
                    "required": ["text"]
                }),
            },
            ToolDefinition {
                name: "browser_press_key".to_string(),
                description: "Press a keyboard key (e.g. Enter, Tab, Escape, ArrowDown).".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Name of the key to press"
                        }
                    },
                    "required": ["key"]
                }),
            },
            ToolDefinition {
                name: "browser_scroll".to_string(),
                description: "Scroll the page by a pixel amount. Positive = down, negative = up. Omit to scroll to bottom.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "amount": {
                            "type": "integer",
                            "description": "Pixels to scroll (positive=down, negative=up). Omit to scroll to bottom."
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_wait".to_string(),
                description: "Wait for a DOM element matching a CSS selector to appear.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "string",
                            "description": "CSS selector to wait for"
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Timeout in milliseconds",
                            "default": 30000
                        }
                    },
                    "required": ["selector"]
                }),
            },
            // --- Tab management ---
            ToolDefinition {
                name: "browser_new_tab".to_string(),
                description: "Open a new tab and navigate to the specified URL.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "URL to open in the new tab"
                        }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "browser_tab_list".to_string(),
                description: "List all open browser tabs with their titles and URLs.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "browser_switch_tab".to_string(),
                description: "Switch to a specific tab by index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Tab index to switch to (0-based)"
                        }
                    },
                    "required": ["index"]
                }),
            },
            ToolDefinition {
                name: "browser_close_tab".to_string(),
                description: "Close the current active tab.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let browser_tool_name = match tool_name {
            "browser_navigate" => "navigate",
            "browser_go_back" => "go_back",
            "browser_go_forward" => "go_forward",
            "browser_close" => "close",
            "browser_extract" => "extract",
            "browser_read_links" => "read_links",
            "browser_get_markdown" => "get_markdown",
            "browser_snapshot" => "snapshot",
            "browser_screenshot" => "screenshot",
            "browser_evaluate" => "evaluate",
            "browser_click" => "click",
            "browser_hover" => "hover",
            "browser_select" => "select",
            "browser_input_fill" => "input",
            "browser_press_key" => "press_key",
            "browser_scroll" => "scroll",
            "browser_wait" => "wait",
            "browser_new_tab" => "new_tab",
            "browser_tab_list" => "tab_list",
            "browser_switch_tab" => "switch_tab",
            "browser_close_tab" => "close_tab",
            _ => {
                return Err(AppError::Tool(format!(
                    "Unknown browser sub-tool: {tool_name}"
                )))
            }
        };

        let result = self
            .session_manager
            .execute_tool(&self.user_id, &self.provider, browser_tool_name, arguments)
            .await?;

        if tool_name == "browser_screenshot"
            && let Ok(parsed) = serde_json::from_str::<Value>(&result)
            && let Some(path) = parsed.get("path").and_then(|p| p.as_str())
            && let Ok(bytes) = std::fs::read(path)
        {
            return Ok(ToolOutput::mixed(result, vec![ImageData {
                bytes,
                media_type: "image/png".into(),
            }]));
        }

        Ok(ToolOutput::text(result))
    }

    async fn cleanup(&self) -> Result<(), AppError> {
        self.session_manager
            .close_session(&self.user_id, &self.provider)
            .await
    }
}
