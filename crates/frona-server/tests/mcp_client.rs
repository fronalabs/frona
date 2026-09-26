use frona::tool::mcp::client::{McpClient, default_client_info};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use tokio::io::duplex;

#[derive(Clone)]
struct EchoServer {
    #[allow(dead_code)]
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

async fn spawn_fake_server() -> McpClient {
    let (client_io, server_io) = duplex(4096);

    tokio::spawn(async move {
        match EchoServer::new().serve(server_io).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => {
                eprintln!("fake server serve() failed: {e}");
            }
        }
    });

    McpClient::connect(client_io, default_client_info())
        .await
        .expect("client connect")
}

#[tokio::test]
async fn connect_seeds_tool_cache() {
    let client = spawn_fake_server().await;

    let cached = client.cached_tools().await;
    let names: Vec<&str> = cached.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"), "missing echo tool: {names:?}");
    assert!(names.contains(&"add"), "missing add tool: {names:?}");

    let echo = cached.iter().find(|t| t.name == "echo").unwrap();
    assert!(
        echo.description.contains("Echo"),
        "echo description should be populated, got {:?}",
        echo.description
    );
    assert!(echo.input_schema.is_object());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn call_tool_returns_text_content() {
    let client = spawn_fake_server().await;

    let result = client
        .call_tool("echo", serde_json::json!({ "text": "hello mcp" }))
        .await
        .expect("call_tool");

    assert_ne!(result.is_error, Some(true));
    assert_eq!(result.content.len(), 1);
    let text = result.content[0]
        .as_text()
        .expect("text content")
        .text
        .clone();
    assert_eq!(text, "hello mcp");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn call_tool_with_multiple_args() {
    let client = spawn_fake_server().await;

    let result = client
        .call_tool("add", serde_json::json!({ "a": 7, "b": 35 }))
        .await
        .expect("call_tool");

    let text = result.content[0]
        .as_text()
        .expect("text content")
        .text
        .clone();
    assert_eq!(text, "42");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn call_unknown_tool_returns_error() {
    let client = spawn_fake_server().await;

    let result = client.call_tool("nonexistent", serde_json::json!({})).await;

    assert!(result.is_err(), "calling unknown tool should error");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn refresh_tools_returns_live_server_state() {
    let client = spawn_fake_server().await;

    let refreshed = client.refresh_tools().await.expect("refresh_tools");
    assert_eq!(refreshed.len(), 2);

    let cached = client.cached_tools().await;
    assert_eq!(cached.len(), 2);

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn peer_info_populated_after_connect() {
    let client = spawn_fake_server().await;

    let info = client.peer_info();
    assert!(
        info.is_some(),
        "peer_info should be populated after initialize"
    );

    client.shutdown().await.unwrap();
}
