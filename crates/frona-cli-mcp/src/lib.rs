mod cli;
mod client;

pub use cli::{CommandOutput, run};
pub use client::{BridgeClient, Error};

/// The CLI and agent tool use the same commands with different transports.
pub trait BridgeBackend: Sync {
    fn list_servers(
        &self,
    ) -> impl std::future::Future<
        Output = Result<Vec<frona_api_types::mcp::BridgeServerInfo>, Error>,
    > + Send;
    fn server_tools(
        &self,
        handle: &str,
    ) -> impl std::future::Future<Output = Result<frona_api_types::mcp::BridgeServerDetail, Error>> + Send;
    fn call_tool(
        &self,
        handle: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<frona_api_types::mcp::BridgeCallResponse, Error>> + Send;
}
