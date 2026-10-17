use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use crate::api::error::ApiError;
use crate::api::middleware::auth::{AuthPrincipal, AuthUser};
use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::tool::mcp::models::{McpServerInstall, McpServerStatus, McpServerUpdate};

async fn validate_request_sandbox_paths(
    state: &AppState,
    auth: &AuthUser,
    policy: Option<&crate::policy::sandbox::SandboxPolicy>,
) -> Result<(), AppError> {
    let Some(policy) = policy else {
        return Ok(());
    };
    let owned_agents: std::collections::HashSet<String> = state
        .agent_service
        .list(&auth.user_id)
        .await?
        .into_iter()
        .map(|a| a.id)
        .collect();
    policy.validate_paths(&auth.handle, |id| owned_agents.contains(id))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mcp/bridge/servers", get(bridge_list_servers))
        .route("/api/mcp/bridge/servers/{slug}", get(bridge_server_tools))
        .route(
            "/api/mcp/bridge/{slug}/call/{tool_name}",
            post(bridge_call_tool),
        )
        .route("/api/mcp/servers", get(list_servers).post(install_server))
        .route(
            "/api/mcp/servers/{id}",
            get(get_server)
                .delete(uninstall_server)
                .patch(update_server),
        )
        .route("/api/mcp/servers/{id}/start", post(start_server))
        .route("/api/mcp/servers/{id}/stop", post(stop_server))
        .route("/api/mcp/servers/{id}/logs", get(get_logs))
        .route("/api/mcp/servers/{id}/logs/stream", get(stream_logs))
        .route("/api/mcp/registry/search", get(search_registry))
        .route("/api/mcp/registry/{name}", get(fetch_registry_entry))
}

#[derive(Serialize)]
struct McpServerResponse {
    id: String,
    handle: String,
    display_name: String,
    description: Option<String>,
    repository_url: Option<String>,
    registry_id: Option<String>,
    status: String,
    command: String,
    args: Vec<String>,
    tool_count: usize,
    active_transport: String,
    transports: Vec<crate::tool::mcp::TransportConfig>,
    env: std::collections::BTreeMap<String, String>,
    /// Set by handlers via `to_response`; not by `From<McpServer>`.
    #[serde(default)]
    sandbox_policy: crate::policy::sandbox::SandboxPolicy,
    installed_at: String,
    last_started_at: Option<String>,
}

impl From<crate::tool::mcp::McpServer> for McpServerResponse {
    fn from(s: crate::tool::mcp::McpServer) -> Self {
        Self {
            id: s.id,
            handle: s.handle.to_string(),
            display_name: s.display_name,
            description: s.description,
            repository_url: s.repository_url,
            registry_id: s.registry_id,
            status: s.status.to_string(),
            command: s.command,
            args: s.args,
            tool_count: s.tool_cache.len(),
            active_transport: s.active_transport,
            transports: s.transports,
            env: s.env,
            sandbox_policy: crate::policy::sandbox::SandboxPolicy::default(),
            installed_at: s.installed_at.to_rfc3339(),
            last_started_at: s.last_started_at.map(|t| t.to_rfc3339()),
        }
    }
}

async fn list_servers(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpServerResponse>>, ApiError> {
    let servers = state.mcp_service.list_for_user(&auth.user_id).await?;
    let mut responses: Vec<McpServerResponse> = Vec::with_capacity(servers.len());
    for s in servers {
        responses.push(to_response(&state, &auth.user_id, &auth.handle, s).await?);
    }
    Ok(Json(responses))
}

async fn to_response(
    state: &AppState,
    user_id: &str,
    user_handle: &crate::core::Handle,
    server: crate::tool::mcp::McpServer,
) -> Result<McpServerResponse, ApiError> {
    let evaluated = state
        .policy_service
        .evaluate_sandbox_policy(
            crate::policy::service::SandboxPrincipalRef::mcp(user_id, user_handle, &server.handle),
            false,
        )
        .await
        .map_err(ApiError::from)?
        .as_ref()
        .clone();
    let mut resp: McpServerResponse = server.into();
    resp.sandbox_policy = evaluated;
    Ok(resp)
}

async fn get_server(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<McpServerResponse>, ApiError> {
    let servers = state.mcp_service.list_for_user(&auth.user_id).await?;
    let server = servers.into_iter().find(|s| s.id == id).ok_or_else(|| {
        ApiError::from(crate::core::error::AppError::NotFound(format!(
            "mcp server {id}"
        )))
    })?;
    let resp = to_response(&state, &auth.user_id, &auth.handle, server).await?;
    Ok(Json(resp))
}

async fn install_server(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<McpServerInstall>,
) -> Result<Json<McpServerResponse>, ApiError> {
    validate_request_sandbox_paths(&state, &auth, req.sandbox_policy.as_ref()).await?;
    let server = state
        .mcp_service
        .install(&auth.user_id, &auth.handle, req)
        .await?;
    let resp = to_response(&state, &auth.user_id, &auth.handle, server).await?;
    Ok(Json(resp))
}

async fn update_server(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<McpServerUpdate>,
) -> Result<Json<UpdateResponse>, ApiError> {
    validate_request_sandbox_paths(&state, &auth, req.sandbox_policy.as_ref()).await?;
    let result = state.mcp_service.update(&auth.user_id, &id, req).await?;
    let server = to_response(&state, &auth.user_id, &auth.handle, result.server).await?;
    Ok(Json(UpdateResponse {
        server,
        restart_required: result.restart_required,
    }))
}

#[derive(Serialize)]
struct UpdateResponse {
    server: McpServerResponse,
    restart_required: bool,
}

async fn uninstall_server(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.mcp_service.uninstall(&auth.user_id, &id).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn start_server(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StartResponse>, ApiError> {
    let result = state.mcp_service.start(&auth.user_id, &id).await?;
    Ok(Json(StartResponse {
        tool_count: result.tools.len(),
        tools: result
            .tools
            .into_iter()
            .map(|t| ToolInfo {
                id: t.id,
                description: t.description,
            })
            .collect(),
    }))
}

#[derive(Serialize)]
struct StartResponse {
    tool_count: usize,
    tools: Vec<ToolInfo>,
}

#[derive(Serialize)]
struct ToolInfo {
    id: String,
    description: String,
}

async fn stop_server(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.mcp_service.stop(&auth.user_id, &id).await?;
    Ok(Json(serde_json::json!({ "stopped": true })))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

async fn resolve_log_path(
    state: &AppState,
    user_id: &str,
    server_id: &str,
) -> Result<std::path::PathBuf, AppError> {
    let server = state.mcp_service.load_owned(user_id, server_id).await?;
    Ok(std::path::PathBuf::from(&server.workspace_dir)
        .join("logs")
        .join("server.log"))
}

async fn get_logs(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let log_path = resolve_log_path(&state, &auth.user_id, &id).await?;
    let logs = crate::tool::mcp::manager::read_log_file(&log_path, 64 * 1024);
    Ok(Json(serde_json::json!({ "logs": logs })))
}

async fn stream_logs(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let log_path = resolve_log_path(&state, &auth.user_id, &id).await?;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Event, Infallible>>();

    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

        let file = loop {
            match tokio::fs::File::open(&log_path).await {
                Ok(f) => break f,
                Err(_) => {
                    if tx.is_closed() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        };

        let mut reader = BufReader::new(file);

        // Send recent context on connect by seeking 8KB from end.
        if let Ok(metadata) = tokio::fs::metadata(&log_path).await {
            let len = metadata.len();
            if len > 8192 {
                let _ = reader.seek(std::io::SeekFrom::End(-8192)).await;
                let mut partial = String::new();
                let _ = reader.read_line(&mut partial).await;
            }
        }

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    if tx.is_closed() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() && tx.send(Ok(Event::default().data(trimmed))).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    return;
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn fetch_registry_entry(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<crate::tool::mcp::RegistryServerEntry>, ApiError> {
    let entry = state.mcp_service.fetch_registry(&name).await?;
    Ok(Json(entry))
}

async fn search_registry(
    _auth: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<crate::tool::mcp::RegistryServerEntry>>, ApiError> {
    let results = state
        .mcp_service
        .search_registry(&query.q, query.limit)
        .await?;
    Ok(Json(results))
}

fn allowed_mcp_tools(
    defs: &[crate::tool::ToolDefinition],
) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
    let mut map: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for def in defs {
        if let Some(rest) = def.id.strip_prefix("mcp__")
            && let Some((slug, tool)) = rest.split_once("__")
        {
            map.entry(slug.to_string())
                .or_default()
                .insert(tool.to_string());
        }
    }
    map
}

async fn bridge_list_servers(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<Vec<frona_api_types::mcp::BridgeServerInfo>>, ApiError> {
    let servers = state.mcp_service.list_for_user(&auth.user_id).await?;
    let running: Vec<_> = servers
        .into_iter()
        .filter(|s| s.status == McpServerStatus::Running)
        .collect();

    let slug_filter = if let Some(agent_id) = auth.agent_id() {
        let agent = state.agent_service.get(&auth.user_id, agent_id).await?;
        let registry = state
            .tool_manager
            .build_agent_registry(&auth.user_id, &agent, &state.policy_service, None)
            .await;
        Some(allowed_mcp_tools(registry.definitions()))
    } else {
        None
    };

    let result = running
        .into_iter()
        .filter(|s| {
            slug_filter
                .as_ref()
                .is_none_or(|f| f.contains_key(s.handle.as_str()))
        })
        .map(|s| {
            let tool_count = s.tool_cache.len();
            frona_api_types::mcp::BridgeServerInfo {
                handle: s.handle.to_string(),
                display_name: s.display_name,
                description: s.description,
                tool_count,
            }
        })
        .collect();

    Ok(Json(result))
}

async fn bridge_server_tools(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(handle): Path<String>,
) -> Result<Json<frona_api_types::mcp::BridgeServerDetail>, ApiError> {
    let servers = state.mcp_service.list_for_user(&auth.user_id).await?;
    let server = servers
        .into_iter()
        .find(|s| s.handle.as_str() == handle && s.status == McpServerStatus::Running)
        .ok_or_else(|| {
            ApiError::from(crate::core::error::AppError::NotFound(format!(
                "MCP server '{handle}'"
            )))
        })?;

    let allowed_tools = if let Some(agent_id) = auth.agent_id() {
        let agent = state.agent_service.get(&auth.user_id, agent_id).await?;
        let registry = state
            .tool_manager
            .build_agent_registry(&auth.user_id, &agent, &state.policy_service, None)
            .await;
        let map = allowed_mcp_tools(registry.definitions());
        map.get(&handle).cloned()
    } else {
        None
    };

    let tools = server
        .tool_cache
        .into_iter()
        .filter(|t| {
            allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&t.name))
        })
        .map(|t| frona_api_types::mcp::BridgeToolInfo {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        })
        .collect();

    Ok(Json(frona_api_types::mcp::BridgeServerDetail {
        handle: server.handle.to_string(),
        display_name: server.display_name,
        description: server.description,
        tools,
    }))
}

async fn bridge_call_tool(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((handle, tool_name)): Path<(String, String)>,
    Json(req): Json<frona_api_types::mcp::BridgeCallRequest>,
) -> Result<Json<frona_api_types::mcp::BridgeCallResponse>, ApiError> {
    if let Some(agent_id) = auth.agent_id() {
        let agent = state.agent_service.get(&auth.user_id, agent_id).await?;
        let registry = state
            .tool_manager
            .build_agent_registry(&auth.user_id, &agent, &state.policy_service, None)
            .await;
        let expected = format!("mcp__{handle}__{tool_name}");
        if !registry.definitions().iter().any(|d| d.id == expected) {
            return Err(ApiError::from(crate::core::error::AppError::Forbidden(
                format!("Agent does not have access to tool '{tool_name}' on server '{handle}'"),
            )));
        }
    }

    let server_id = state
        .mcp_manager
        .find_by_handle(&auth.user_id, &handle)
        .await
        .ok_or_else(|| {
            ApiError::from(crate::core::error::AppError::NotFound(format!(
                "Running MCP server '{handle}'"
            )))
        })?;

    let result = state
        .mcp_manager
        .call(&server_id, &tool_name, req.arguments)
        .await?;

    let is_error = result.is_error.unwrap_or(false);
    let content = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(Json(frona_api_types::mcp::BridgeCallResponse {
        content,
        is_error,
    }))
}
