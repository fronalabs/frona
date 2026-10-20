use std::sync::{Arc, Weak};

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    ServerPeerInfo, Tool,
};
use rmcp::service::{NotificationContext, RoleClient, RunningService};
use rmcp::transport::IntoTransport;
use rmcp::{ClientHandler, ErrorData as McpError};
use tokio::sync::{Mutex, RwLock};

use crate::core::error::AppError;

use super::models::CachedMcpTool;
use super::service::McpServerService;
use crate::tool::ToolDefinition;

struct ToolRegistration {
    service: Weak<McpServerService>,
    user_id: String,
    handle: String,
}

impl ToolRegistration {
    async fn publish(&self, tools: &[CachedMcpTool]) {
        if let Some(service) = self.service.upgrade() {
            service
                .register_tools(
                    &self.user_id,
                    &self.handle,
                    tool_definitions(&self.handle, tools),
                )
                .await;
        }
    }
}

/// Handler passed into `rmcp::ServiceExt::serve`. Receives server-initiated notifications
/// and keeps an in-memory mirror of the server's current tool list so `on_tool_list_changed`
/// can refresh it without racing the tool-calling path.
pub struct McpClientHandler {
    cached_tools: Arc<RwLock<Vec<CachedMcpTool>>>,
    client_info: ClientConfig,
    registration: Arc<Mutex<Option<ToolRegistration>>>,
}

impl McpClientHandler {
    fn new(
        client_info: ClientConfig,
        cached_tools: Arc<RwLock<Vec<CachedMcpTool>>>,
        registration: Arc<Mutex<Option<ToolRegistration>>>,
    ) -> Self {
        Self {
            cached_tools,
            client_info,
            registration,
        }
    }
}

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> ClientConfig {
        self.client_info.clone()
    }

    async fn on_tool_list_changed(&self, ctx: NotificationContext<RoleClient>) {
        // Serialize fetch, publication and shutdown so an old response cannot
        // replace a newer registration or restore tools after stop.
        let registration = self.registration.lock().await;
        match ctx.peer.list_all_tools().await {
            Ok(tools) => {
                let converted: Vec<CachedMcpTool> = tools
                    .into_iter()
                    .map(cached_from_rmcp_tool)
                    .collect::<Vec<_>>();
                *self.cached_tools.write().await = converted.clone();
                if let Some(registration) = registration.as_ref() {
                    registration.publish(&converted).await;
                }
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
    registration: Arc<Mutex<Option<ToolRegistration>>>,
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
        let registration = Arc::new(Mutex::new(None));
        let handler =
            McpClientHandler::new(client_info, cached_tools.clone(), registration.clone());

        let running = handler
            .serve(transport)
            .await
            .map_err(|e| AppError::Tool(format!("MCP initialize failed: {e}")))?;

        let refresh = registration.lock().await;
        let tools = running
            .list_all_tools()
            .await
            .map_err(|e| AppError::Tool(format!("MCP tools/list failed: {e}")))?;
        *cached_tools.write().await = tools.into_iter().map(cached_from_rmcp_tool).collect();

        drop(refresh);
        Ok(Self {
            running,
            cached_tools,
            registration,
        })
    }

    /// Fresh `tools/list` from the server, bypassing the cache. Updates the cache on
    /// success.
    pub async fn refresh_tools(&self) -> Result<Vec<CachedMcpTool>, AppError> {
        let registration = self.registration.lock().await;
        let tools = self
            .running
            .list_all_tools()
            .await
            .map_err(|e| AppError::Tool(format!("MCP tools/list failed: {e}")))?;
        let converted: Vec<CachedMcpTool> = tools.into_iter().map(cached_from_rmcp_tool).collect();
        *self.cached_tools.write().await = converted.clone();
        if let Some(registration) = registration.as_ref() {
            registration.publish(&converted).await;
        }
        Ok(converted)
    }

    /// Attach the existing per-user tool registry after the connection is started.
    /// Publishing the latest cache also covers notifications received during startup.
    pub(super) async fn register_tools(
        &self,
        service: Weak<McpServerService>,
        user_id: &str,
        handle: &str,
    ) {
        let mut registration = self.registration.lock().await;
        let target = ToolRegistration {
            service,
            user_id: user_id.into(),
            handle: handle.into(),
        };
        target.publish(&self.cached_tools().await).await;
        *registration = Some(target);
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
        // Cancel pending tools/list before waiting for its registration lock.
        // Once the client has stopped, no notification can restore removed tools.
        let result = self.running.cancel().await;
        if let Some(target) = self.registration.lock().await.take() {
            target.publish(&[]).await;
        }
        result.map_err(|e| AppError::Tool(format!("MCP shutdown failed: {e}")))?;
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

pub(super) fn tool_definitions(handle: &str, tools: &[CachedMcpTool]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| ToolDefinition {
            id: format!("mcp__{handle}__{}", tool.name),
            provider_id: format!("mcp:{handle}"),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        })
        .collect()
}
