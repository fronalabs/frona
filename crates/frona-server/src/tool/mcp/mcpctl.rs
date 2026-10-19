use frona_derive::agent_tool;
use serde_json::Value;

use super::service::McpServerService;
use crate::{
    agent::prompt::PromptLoader,
    core::error::AppError,
    tool::{InferenceContext, ToolOutput},
};
use std::sync::Arc;

pub struct McpCtlTool {
    service: Arc<McpServerService>,
    pub prompts: PromptLoader,
}

impl McpCtlTool {
    pub fn new(service: Arc<McpServerService>, prompts: PromptLoader) -> Self {
        Self { service, prompts }
    }
}

struct DirectBridge<'a> {
    service: &'a McpServerService,
    user_id: &'a str,
    agent_id: &'a str,
}

fn bridge_error(error: AppError) -> frona_cli_mcp::Error {
    let public = crate::api::error::ApiError(error).public_error();
    frona_cli_mcp::Error::Api {
        status: public.status.as_u16(),
        body: public.message,
    }
}

impl frona_cli_mcp::BridgeBackend for DirectBridge<'_> {
    async fn list_servers(
        &self,
    ) -> Result<Vec<frona_api_types::mcp::BridgeServerInfo>, frona_cli_mcp::Error> {
        self.service
            .bridge_list_servers(self.user_id, Some(self.agent_id))
            .await
            .map_err(bridge_error)
    }
    async fn server_tools(
        &self,
        handle: &str,
    ) -> Result<frona_api_types::mcp::BridgeServerDetail, frona_cli_mcp::Error> {
        self.service
            .bridge_server_tools(self.user_id, Some(self.agent_id), handle)
            .await
            .map_err(bridge_error)
    }
    async fn call_tool(
        &self,
        handle: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<frona_api_types::mcp::BridgeCallResponse, frona_cli_mcp::Error> {
        self.service
            .bridge_call_tool(
                self.user_id,
                Some(self.agent_id),
                handle,
                tool_name,
                arguments,
            )
            .await
            .map_err(bridge_error)
    }
}

#[agent_tool(name = "mcpctl")]
impl McpCtlTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let Some(object) = arguments.as_object() else {
            return Ok(ToolOutput::error("Expected args: string[]"));
        };
        if object
            .keys()
            .any(|key| key != "args" && key != "description")
        {
            return Ok(ToolOutput::error(
                "mcpctl accepts only args and description; Frona uses the current agent and user context",
            ));
        }
        let args = match object
            .get("args")
            .cloned()
            .map(serde_json::from_value::<Vec<String>>)
        {
            Some(Ok(args)) => args,
            _ => return Ok(ToolOutput::error("Expected args: string[]")),
        };
        if args.len() > 256 || args.iter().map(String::len).sum::<usize>() > 64 * 1024 {
            return Ok(ToolOutput::error(
                "mcpctl arguments exceed the limit (256 arguments or 64 KiB)",
            ));
        }
        let timeout = ctx
            .agent
            .sandbox_limits
            .as_ref()
            .map(|limits| limits.timeout_secs)
            .filter(|seconds| *seconds > 0)
            .unwrap_or(120)
            .min(120);
        let invoke = async {
            let client = DirectBridge {
                service: &self.service,
                user_id: &ctx.user.id,
                agent_id: &ctx.agent.id,
            };
            let args = std::iter::once("mcpctl".to_string()).chain(args).collect();
            let output = frona_cli_mcp::run(client, args).await;
            let mut text = output.stdout;
            if !output.stderr.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("stderr:\n");
                text.push_str(&output.stderr);
            }
            Ok(if output.success {
                ToolOutput::text(text)
            } else {
                ToolOutput::error(text)
            })
        };
        tokio::select! {
            biased;
            _ = ctx.cancel_token.cancelled() => Ok(ToolOutput::error("mcpctl cancelled")),
            _ = ctx.shutdown_token.cancelled() => Ok(ToolOutput::error("mcpctl cancelled: server shutdown")),
            _ = tokio::time::sleep(std::time::Duration::from_secs(timeout)) => Ok(ToolOutput::error(format!("mcpctl timed out after {timeout} seconds"))),
            result = invoke => result,
        }
    }
}
