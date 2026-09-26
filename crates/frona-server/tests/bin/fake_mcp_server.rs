//! Minimal MCP server over stdio for end-to-end testing. Exposes `echo` and
//! `add` tools, runs until stdin is closed.
//!
//! Build: `cargo build -p frona --bin fake-mcp-server --features __test-bins`
//! Run:   `./target/debug/fake-mcp-server`

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};

#[derive(Clone)]
struct EchoServer {
    tool_router: ToolRouter<EchoServer>,
}

#[tool_router]
impl EchoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo back the provided text.")]
    fn echo(&self, Parameters(args): Parameters<JsonObject>) -> Result<CallToolResult, McpError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(description = "Add two integers and return their sum.")]
    fn add(&self, Parameters(args): Parameters<JsonObject>) -> Result<CallToolResult, McpError> {
        let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
        let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            (a + b).to_string(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let server = EchoServer::new();
    match server.serve((stdin, stdout)).await {
        Ok(running) => {
            let _ = running.waiting().await;
        }
        Err(e) => {
            eprintln!("fake-mcp-server error: {e}");
            std::process::exit(1);
        }
    }
}
