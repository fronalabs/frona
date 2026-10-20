use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::core::error::AppError;
use crate::credential::vault::service::VaultService;
use crate::tool::{AgentTool, ImageData, InferenceContext, ToolDefinition, ToolOutput};

use super::session::{BrowserSessionManager, run_with_reconnect};
use frona_browser::{ElementTarget, ExtractFormat};

pub struct BrowserTool {
    session_manager: Arc<BrowserSessionManager>,
    vault_service: VaultService,
}

impl BrowserTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>, vault_service: VaultService) -> Self {
        Self {
            session_manager,
            vault_service,
        }
    }
}

fn default_true() -> bool {
    true
}

fn element_target(
    selector: Option<&str>,
    index: Option<usize>,
) -> Result<ElementTarget<'_>, AppError> {
    match (selector, index) {
        (Some(s), None) => Ok(ElementTarget::Selector(s)),
        (None, Some(i)) => Ok(ElementTarget::Index(i)),
        (Some(_), Some(_)) => Err(AppError::Validation(
            "specify either selector or index, not both".into(),
        )),
        (None, None) => Err(AppError::Validation(
            "must specify selector or index".into(),
        )),
    }
}

#[async_trait]
impl AgentTool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_navigate".to_string(),
                description: "Navigate to a URL in the browser.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "URL to navigate to" },
                        "wait_for_load": { "type": "boolean", "description": "Wait for navigation to complete", "default": true }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_go_back".to_string(),
                description: "Navigate back in browser history.".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_go_forward".to_string(),
                description: "Navigate forward in browser history.".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_close".to_string(),
                description: "Close the browser when the task is complete.".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_extract".to_string(),
                description: "Extract text or HTML content from the current page or a specific element.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {"type":"string","description":"CSS selector to extract from (defaults to body)"},
                        "format": {"type":"string","enum":["text","html"],"description":"Output format","default":"text"}
                    }
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_read_links".to_string(),
                description: "Get all links on the current page with their text and URLs.".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_get_markdown".to_string(),
                description: "Get the markdown content of the current page with pagination support.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "page": {"type":"integer","minimum":1,"description":"Page number to extract (1-based)","default":1},
                        "page_size": {"type":"integer","minimum":1,"description":"Maximum characters per page","default":100000}
                    }
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_snapshot".to_string(),
                description: "Get an ARIA snapshot of the page with indexed interactive elements.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "incremental": {"type":"boolean","description":"Return unified diff against the previous snapshot instead of full tree","default":false},
                        "compact": {"type":"boolean","description":"Strip non-actionable lines from the output to save tokens","default":false}
                    }
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_screenshot".to_string(),
                description: "Capture a screenshot of the current page.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type":"string","description":"Path to save the screenshot"},
                        "full_page": {"type":"boolean","description":"Capture full page instead of viewport","default":false}
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_evaluate".to_string(),
                description: "Execute JavaScript code in the browser context.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "code": {"type":"string","description":"JavaScript code to execute"},
                        "await_promise": {"type":"boolean","description":"Wait for promise resolution","default":false}
                    },
                    "required": ["code"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_click".to_string(),
                description: "Click on a DOM element by CSS selector or snapshot index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {"type":"string","description":"CSS selector of the element to click"},
                        "index": {"type":"integer","minimum":0,"description":"Snapshot index of the element to click"}
                    }
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_hover".to_string(),
                description: "Hover over a DOM element by CSS selector or snapshot index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {"type":"string","description":"CSS selector of the element to hover"},
                        "index": {"type":"integer","minimum":0,"description":"Snapshot index of the element to hover"}
                    }
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_select".to_string(),
                description: "Select an option in a dropdown element.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {"type":"string","description":"CSS selector of the dropdown"},
                        "index": {"type":"integer","minimum":0,"description":"Snapshot index of the dropdown"},
                        "value": {"type":"string","description":"Value to select in the dropdown"}
                    },
                    "required": ["value"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_input_fill".to_string(),
                description: "Type text into an input element by CSS selector or snapshot index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {"type":"string","description":"CSS selector of the input element"},
                        "index": {"type":"integer","minimum":0,"description":"Snapshot index of the input element"},
                        "text": {"type":"string","description":"Text to type into the element"},
                        "clear": {"type":"boolean","description":"Clear existing content first","default":false}
                    },
                    "required": ["text"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_press_key".to_string(),
                description: "Press a keyboard key (e.g. Enter, Tab, Escape, ArrowDown).".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"key": {"type":"string","description":"Name of the key to press"}},
                    "required": ["key"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_scroll".to_string(),
                description: "Scroll the page by a pixel amount. Positive = down, negative = up. Omit to scroll to bottom.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"amount": {"type":"integer","description":"Pixels to scroll (positive=down, negative=up). Omit to scroll to bottom."}}
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_wait".to_string(),
                description: "Wait for a DOM element matching a CSS selector to appear.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {"type":"string","description":"CSS selector to wait for"},
                        "timeout_ms": {"type":"integer","minimum":0,"description":"Timeout in milliseconds","default":30000}
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_new_tab".to_string(),
                description: "Open a new tab and navigate to the specified URL.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"url": {"type":"string","description":"URL to open in the new tab"}},
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_tab_list".to_string(),
                description: "List all open browser tabs with their titles and URLs.".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_switch_tab".to_string(),
                description: "Switch to a specific tab by index.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"index": {"type":"integer","minimum":0,"description":"Tab index to switch to (0-based)"}},
                    "required": ["index"]
                }),
            },
            ToolDefinition {
                provider_id: "browser".to_string(),
                id: "browser_close_tab".to_string(),
                description: "Close the current active tab.".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
        ]
    }

    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let session_key = &ctx.user.handle;
        let provider = self
            .vault_service
            .list_credentials(&ctx.user.id)
            .await
            .ok()
            .and_then(|creds| creds.into_iter().next())
            .map(|c| c.provider)
            .unwrap_or_else(|| "default".to_string());
        let mgr = self.session_manager.as_ref();

        match tool_name {
            "browser_navigate" => {
                #[derive(Deserialize)]
                struct P {
                    url: String,
                    #[serde(default = "default_true")]
                    wait_for_load: bool,
                }
                let p: P = serde_json::from_value(arguments)?;
                let url = p.url.as_str();
                let wait = p.wait_for_load;
                let info = run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.navigate(url, wait).await
                })
                .await?;
                Ok(ToolOutput::text(serde_json::to_string(&info)?))
            }
            "browser_go_back" => {
                run_with_reconnect(mgr, ctx, &provider, |c| async move { c.go_back().await })
                    .await?;
                Ok(ToolOutput::text(String::new()))
            }
            "browser_go_forward" => {
                run_with_reconnect(mgr, ctx, &provider, |c| async move { c.go_forward().await })
                    .await?;
                Ok(ToolOutput::text(String::new()))
            }
            "browser_close" => {
                tokio::select! {
                    biased;
                    _ = ctx.cancel_token.cancelled() => return Err(AppError::Tool("Browser operation cancelled".into())),
                    result = mgr.close_session(session_key, &provider) => result?,
                }
                Ok(ToolOutput::text("Browser closed."))
            }
            "browser_extract" => {
                #[derive(Deserialize)]
                struct P {
                    selector: Option<String>,
                    #[serde(default)]
                    format: Option<String>,
                }
                let p: P = serde_json::from_value(arguments)?;
                let format = match p.format.as_deref() {
                    Some("html") => ExtractFormat::Html,
                    _ => ExtractFormat::Text,
                };
                let selector = p.selector.as_deref();
                let content = run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.extract(selector, format).await
                })
                .await?;
                Ok(ToolOutput::text(content))
            }
            "browser_read_links" => {
                let links =
                    run_with_reconnect(
                        mgr,
                        ctx,
                        &provider,
                        |c| async move { c.read_links().await },
                    )
                    .await?;
                Ok(ToolOutput::text(serde_json::to_string(&links)?))
            }
            "browser_get_markdown" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default = "page_default")]
                    page: usize,
                    #[serde(default = "page_size_default")]
                    page_size: usize,
                }
                fn page_default() -> usize {
                    1
                }
                fn page_size_default() -> usize {
                    100_000
                }
                let p: P = serde_json::from_value(arguments).unwrap_or(P {
                    page: 1,
                    page_size: 100_000,
                });
                let md = run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.get_markdown(p.page, p.page_size).await
                })
                .await?;
                Ok(ToolOutput::text(serde_json::to_string(&md)?))
            }
            "browser_snapshot" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    incremental: bool,
                    #[serde(default)]
                    compact: bool,
                }
                let p: P = serde_json::from_value(arguments).unwrap_or(P {
                    incremental: false,
                    compact: false,
                });
                let snap = run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.snapshot(p.incremental, p.compact).await
                })
                .await?;
                Ok(ToolOutput::text(serde_json::to_string(&snap)?))
            }
            "browser_screenshot" => {
                #[derive(Deserialize)]
                struct P {
                    path: String,
                    #[serde(default)]
                    full_page: bool,
                }
                let p: P = serde_json::from_value(arguments)?;
                let path = Path::new(&p.path);
                let full_page = p.full_page;
                let result = run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.screenshot(path, full_page).await
                })
                .await?;
                let text = serde_json::to_string(&result)?;
                if let Ok(bytes) = std::fs::read(&p.path) {
                    return Ok(ToolOutput::mixed(
                        text,
                        vec![ImageData {
                            bytes,
                            media_type: "image/png".into(),
                        }],
                    ));
                }
                Ok(ToolOutput::text(text))
            }
            "browser_evaluate" => {
                #[derive(Deserialize)]
                struct P {
                    code: String,
                    #[serde(default)]
                    await_promise: bool,
                }
                let p: P = serde_json::from_value(arguments)?;
                let code = p.code.as_str();
                let await_promise = p.await_promise;
                let value = run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.evaluate(code, await_promise).await
                })
                .await?;
                Ok(ToolOutput::text(serde_json::to_string(&value)?))
            }
            "browser_click" => {
                #[derive(Deserialize)]
                struct P {
                    selector: Option<String>,
                    index: Option<usize>,
                }
                let p: P = serde_json::from_value(arguments)?;
                let target = element_target(p.selector.as_deref(), p.index)?;
                run_with_reconnect(
                    mgr,
                    ctx,
                    &provider,
                    |c| async move { c.click(target).await },
                )
                .await?;
                Ok(ToolOutput::text("clicked"))
            }
            "browser_hover" => {
                #[derive(Deserialize)]
                struct P {
                    selector: Option<String>,
                    index: Option<usize>,
                }
                let p: P = serde_json::from_value(arguments)?;
                let target = element_target(p.selector.as_deref(), p.index)?;
                run_with_reconnect(
                    mgr,
                    ctx,
                    &provider,
                    |c| async move { c.hover(target).await },
                )
                .await?;
                Ok(ToolOutput::text("hovered"))
            }
            "browser_select" => {
                #[derive(Deserialize)]
                struct P {
                    selector: Option<String>,
                    index: Option<usize>,
                    value: String,
                }
                let p: P = serde_json::from_value(arguments)?;
                let target = element_target(p.selector.as_deref(), p.index)?;
                let value = p.value.as_str();
                run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.select(target, value).await
                })
                .await?;
                Ok(ToolOutput::text("selected"))
            }
            "browser_input_fill" => {
                #[derive(Deserialize)]
                struct P {
                    selector: Option<String>,
                    index: Option<usize>,
                    text: String,
                    #[serde(default)]
                    clear: bool,
                }
                let p: P = serde_json::from_value(arguments)?;
                let target = element_target(p.selector.as_deref(), p.index)?;
                let text = p.text.as_str();
                let clear = p.clear;
                run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.input_fill(target, text, clear).await
                })
                .await?;
                Ok(ToolOutput::text("input filled"))
            }
            "browser_press_key" => {
                #[derive(Deserialize)]
                struct P {
                    key: String,
                }
                let p: P = serde_json::from_value(arguments)?;
                let key = p.key.as_str();
                run_with_reconnect(
                    mgr,
                    ctx,
                    &provider,
                    |c| async move { c.press_key(key).await },
                )
                .await?;
                Ok(ToolOutput::text(format!("pressed {}", p.key)))
            }
            "browser_scroll" => {
                #[derive(Deserialize)]
                struct P {
                    amount: Option<i64>,
                }
                let p: P = serde_json::from_value(arguments).unwrap_or(P { amount: None });
                run_with_reconnect(
                    mgr,
                    ctx,
                    &provider,
                    |c| async move { c.scroll(p.amount).await },
                )
                .await?;
                Ok(ToolOutput::text("scrolled"))
            }
            "browser_wait" => {
                #[derive(Deserialize)]
                struct P {
                    selector: String,
                    #[serde(default = "wait_default")]
                    timeout_ms: u64,
                }
                fn wait_default() -> u64 {
                    30000
                }
                let p: P = serde_json::from_value(arguments)?;
                let timeout = Duration::from_millis(p.timeout_ms);
                let selector = p.selector.as_str();
                run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.wait_for_selector(selector, timeout).await
                })
                .await?;
                Ok(ToolOutput::text(format!("found {}", p.selector)))
            }
            "browser_new_tab" => {
                #[derive(Deserialize)]
                struct P {
                    url: String,
                }
                let p: P = serde_json::from_value(arguments)?;
                let url = p.url.as_str();
                let info =
                    run_with_reconnect(
                        mgr,
                        ctx,
                        &provider,
                        |c| async move { c.new_tab(url).await },
                    )
                    .await?;
                Ok(ToolOutput::text(serde_json::to_string(&info)?))
            }
            "browser_tab_list" => {
                let tabs =
                    run_with_reconnect(mgr, ctx, &provider, |c| async move { c.tabs().await })
                        .await?;
                Ok(ToolOutput::text(serde_json::to_string(&tabs)?))
            }
            "browser_switch_tab" => {
                #[derive(Deserialize)]
                struct P {
                    index: usize,
                }
                let p: P = serde_json::from_value(arguments)?;
                run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.switch_tab(p.index).await
                })
                .await?;
                Ok(ToolOutput::text(format!("switched to tab {}", p.index)))
            }
            "browser_close_tab" => {
                run_with_reconnect(mgr, ctx, &provider, |c| async move {
                    c.close_active_tab().await
                })
                .await?;
                Ok(ToolOutput::text("tab closed"))
            }
            other => Err(AppError::Tool(format!("Unknown browser sub-tool: {other}"))),
        }
    }

    async fn cleanup(&self) -> Result<(), AppError> {
        Ok(())
    }
}
