use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    ServerPeerInfo, Tool,
};
use rmcp::service::{NotificationContext, RoleClient, RunningService};
use rmcp::transport::IntoTransport;
use rmcp::{ClientHandler, ErrorData as McpError};
use tokio::sync::RwLock;

use crate::core::error::AppError;

use super::models::CachedMcpTool;

/// Handler passed into `rmcp::ServiceExt::serve`. Receives server-initiated notifications
/// and keeps an in-memory mirror of the server's current tool list so `on_tool_list_changed`
/// can refresh it without racing the tool-calling path.
pub struct McpClientHandler {
    cached_tools: Arc<RwLock<Vec<CachedMcpTool>>>,
    client_info: ClientConfig,
}

impl McpClientHandler {
    pub fn new(client_info: ClientConfig, cached_tools: Arc<RwLock<Vec<CachedMcpTool>>>) -> Self {
        Self {
            cached_tools,
            client_info,
        }
    }
}

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> ClientConfig {
        self.client_info.clone()
    }

    async fn on_tool_list_changed(&self, ctx: NotificationContext<RoleClient>) {
        match ctx.peer.list_all_tools().await {
            Ok(tools) => {
                let converted: Vec<CachedMcpTool> = tools
                    .into_iter()
                    .map(cached_from_rmcp_tool)
                    .collect::<Vec<_>>();
                *self.cached_tools.write().await = converted;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to refresh MCP tool list after notification");
            }
        }
    }

    async fn ping(&self, _ctx: rmcp::service::RequestContext<RoleClient>) -> Result<(), McpError> {
        Ok(())
    }
}

/// Thin wrapper around a live `rmcp` client connection to an MCP server. Holds the
/// `RunningService` (keeping the child alive via the transport) and exposes a small
/// Frona-flavoured surface over `list_tools` / `call_tool`.
pub struct McpClient {
    running: RunningService<RoleClient, McpClientHandler>,
    cached_tools: Arc<RwLock<Vec<CachedMcpTool>>>,
}

impl McpClient {
    /// Perform the MCP `initialize` handshake over the given transport, then fetch the
    /// initial `tools/list` and seed the cache. Returns an `McpClient` whose lifetime
    /// keeps the underlying connection alive.
    pub async fn connect<T, E, A>(transport: T, client_info: ClientConfig) -> Result<Self, AppError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let cached_tools = Arc::new(RwLock::new(Vec::new()));
        let handler = McpClientHandler::new(client_info, cached_tools.clone());

        let running = handler
            .serve(transport)
            .await
            .map_err(|e| AppError::Tool(format!("MCP initialize failed: {e}")))?;

        let tools = running
            .list_all_tools()
            .await
            .map_err(|e| AppError::Tool(format!("MCP tools/list failed: {e}")))?;
        *cached_tools.write().await = tools.into_iter().map(cached_from_rmcp_tool).collect();

        Ok(Self {
            running,
            cached_tools,
        })
    }

    /// Fresh `tools/list` from the server, bypassing the cache. Updates the cache on
    /// success.
    pub async fn refresh_tools(&self) -> Result<Vec<CachedMcpTool>, AppError> {
        let tools = self
            .running
            .list_all_tools()
            .await
            .map_err(|e| AppError::Tool(format!("MCP tools/list failed: {e}")))?;
        let converted: Vec<CachedMcpTool> = tools.into_iter().map(cached_from_rmcp_tool).collect();
        *self.cached_tools.write().await = converted.clone();
        Ok(converted)
    }

    pub async fn cached_tools(&self) -> Vec<CachedMcpTool> {
        self.cached_tools.read().await.clone()
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, AppError> {
        let mut params = CallToolRequestParams::new(name.to_string());
        if let serde_json::Value::Object(map) = arguments {
            params = params.with_arguments(map);
        }
        self.running
            .call_tool(params)
            .await
            .map_err(|e| AppError::Tool(format!("MCP call_tool failed: {e}")))
    }

    pub fn peer_info(&self) -> Option<ServerPeerInfo> {
        self.running.peer_info().map(|info| (*info).clone())
    }

    pub async fn shutdown(self) -> Result<(), AppError> {
        self.running
            .cancel()
            .await
            .map_err(|e| AppError::Tool(format!("MCP shutdown failed: {e}")))?;
        Ok(())
    }
}

pub fn default_client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("frona", env!("CARGO_PKG_VERSION")),
    )
}

fn cached_from_rmcp_tool(tool: Tool) -> CachedMcpTool {
    let input_schema = serde_json::Value::Object((*tool.input_schema).clone());
    CachedMcpTool {
        name: tool.name.into_owned(),
        description: tool.description.map(|c| c.into_owned()).unwrap_or_default(),
        input_schema,
    }
}
