//! Real list_changed notifications replace the existing per-user registration.
mod helpers;

use frona::{
    core::repository::Repository,
    db::repo::generic::SurrealRepo,
    tool::{InferenceContext, ToolDefinition, mcp::models::*, registry::AgentToolRegistry},
};
use rmcp::{
    ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, ServerCapabilities,
        ServerConfig, Tool,
    },
    service::{NotificationContext, RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, Notify, RwLock};

#[derive(Default)]
struct DynamicServer {
    tools: RwLock<Vec<Tool>>,
    peer: Mutex<Option<rmcp::Peer<RoleServer>>>,
    fail_list: AtomicBool,
    failed: Notify,
    block_next: AtomicBool,
    started: Notify,
    release: Notify,
}

impl ServerHandler for DynamicServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    async fn on_initialized(&self, ctx: NotificationContext<RoleServer>) {
        *self.peer.lock().await = Some(ctx.peer);
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        if self.fail_list.load(Ordering::SeqCst) {
            self.failed.notify_one();
            return Err(rmcp::ErrorData::internal_error("fixture failure", None));
        }
        let tools = self.tools.read().await.clone();
        if self.block_next.swap(false, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(request.name)]).into())
    }
}

fn tool(name: &str, version: usize) -> Tool {
    Tool::new(
        name.to_string(),
        format!("version {version}"),
        Arc::new(
            json!({
                "type":"object", "properties":{format!("value{version}"):{"type":"string"}}
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

struct Fixture {
    app: helpers::app_state::Fixture,
    ctx: InferenceContext,
    remote: Arc<DynamicServer>,
    http: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        frona::db::init::setup_schema(&db).await.unwrap();
        let app = helpers::app_state::build(&db).await;
        let ctx = helpers::mock_context();
        app.state.user_service.create(&ctx.user).await.unwrap();
        SurrealRepo::<frona::agent::models::Agent>::new(db.clone())
            .create(&ctx.agent)
            .await
            .unwrap();
        app.state.policy_service.sync_base_policies().await.unwrap();
        let remote = Arc::new(DynamicServer::default());
        let factory = remote.clone();
        let service = StreamableHttpService::new(
            move || Ok(factory.clone()),
            Arc::new(LocalSessionManager::default()),
            Default::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let http = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
                .await
                .unwrap();
        });
        let now = chrono::Utc::now();
        SurrealRepo::<McpServer>::new(db)
            .create(&McpServer {
                id: "dynamic".into(),
                user_id: ctx.user.id.clone(),
                handle: frona::handle!("dynamic"),
                display_name: "Dynamic".into(),
                description: None,
                repository_url: None,
                registry_id: None,
                server_info: None,
                package: McpPackage {
                    runtime: McpRuntime::Binary,
                    name: "fixture".into(),
                    version: "1".into(),
                },
                command: String::new(),
                args: vec![],
                env: Default::default(),
                transports: vec![TransportConfig::Http {
                    args: vec![],
                    env: Default::default(),
                    port_env_var: None,
                    endpoint_path: None,
                    url: Some(url),
                }],
                active_transport: "streamable-http".into(),
                status: McpServerStatus::Installed,
                tool_cache: vec![],
                workspace_dir: app.state.config.storage.data_dir.clone() + "/dynamic",
                installed_at: now,
                last_started_at: None,
                updated_at: now,
            })
            .await
            .unwrap();
        Self {
            app,
            ctx,
            remote,
            http,
        }
    }

    async fn start(&self) {
        self.app
            .state
            .mcp_service
            .start(&self.ctx.user.id, "dynamic")
            .await
            .unwrap();
    }

    async fn registry(&self) -> AgentToolRegistry {
        let mut registry = self
            .app
            .state
            .tool_manager
            .build_agent_registry(
                &self.ctx.user.id,
                &self.ctx.agent,
                &self.app.state.policy_service,
                None,
            )
            .await;
        self.app
            .state
            .mcp_service
            .register_agent_tool(&mut registry, &self.app.state.prompts);
        registry
    }

    async fn publish(&self, tools: Vec<Tool>) {
        *self.remote.tools.write().await = tools;
        self.remote
            .peer
            .lock()
            .await
            .as_ref()
            .unwrap()
            .notify_tool_list_changed()
            .await
            .unwrap();
    }

    async fn wait_tools(&self, expected: &[(&str, usize)]) -> Vec<ToolDefinition> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let tools: Vec<_> = self
                    .app
                    .state
                    .tool_manager
                    .definitions(&self.ctx.user.id)
                    .await
                    .into_iter()
                    .filter(|tool| tool.provider_id == "mcp:dynamic")
                    .collect();
                if tools.len() == expected.len()
                    && expected.iter().all(|(name, version)| {
                        tools.iter().any(|tool| {
                            tool.id == format!("mcp__dynamic__{name}")
                                && tool.description == format!("version {version}")
                        })
                    })
                {
                    return tools;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("notification updates registration")
    }

    async fn dynamic_read_allowed(&self) -> bool {
        self.app
            .state
            .policy_service
            .evaluate_sandbox_policy(
                frona::policy::service::SandboxPrincipalRef::agent(
                    &self.ctx.user.id,
                    &self.ctx.user.handle,
                    &self.ctx.agent.handle,
                ),
                false,
            )
            .await
            .unwrap()
            .read_paths
            .contains(&"/dynamic-only".to_string())
    }

    async fn finish(self) {
        self.app
            .state
            .mcp_service
            .stop(&self.ctx.user.id, "dynamic")
            .await
            .unwrap();
        self.http.abort();
    }
}

#[tokio::test]
async fn notification_replaces_registration_and_bridge_discovery_with_fresh_permissions() {
    let fixture = Fixture::new().await;
    fixture.start().await;
    let old_registry = fixture.registry().await;
    fixture.app.state.policy_service.create_policy(&fixture.ctx.user.id,
        "@id(\"private\") forbid(principal, action, resource == Policy::Tool::\"mcp__dynamic__private\");"
    ).await.unwrap();
    fixture.app.state.policy_service.create_policy(&fixture.ctx.user.id,
        "@id(\"dynamic-read\") permit(principal, action == Policy::Action::\"read\", resource == Policy::Path::\"/dynamic-only\") when { principal.tools.contains(\"mcp__dynamic__echo\") };"
    ).await.unwrap();
    assert!(!fixture.dynamic_read_allowed().await); // Warm the membership cache before addition.
    fixture
        .publish(vec![tool("echo", 1), tool("private", 1)])
        .await;
    fixture.wait_tools(&[("echo", 1), ("private", 1)]).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !fixture.dynamic_read_allowed().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let first = fixture.registry().await;
    assert!(
        first
            .definitions()
            .iter()
            .any(|tool| tool.id == "mcp__dynamic__echo")
    );
    assert!(
        !first
            .definitions()
            .iter()
            .any(|tool| tool.id == "mcp__dynamic__private")
    );
    assert!(
        !old_registry
            .definitions()
            .iter()
            .any(|tool| tool.id.starts_with("mcp__dynamic__"))
    );
    assert!(
        first
            .execute("mcp__dynamic__echo", json!({}), &fixture.ctx)
            .await
            .unwrap()
            .is_success()
    );
    assert!(
        first
            .execute("mcpctl", json!({"args":["dynamic","echo"]}), &fixture.ctx)
            .await
            .unwrap()
            .is_success()
    );
    assert!(
        fixture
            .app
            .state
            .mcp_manager
            .server_for_tool("another-user", "mcp__dynamic__echo")
            .await
            .is_none()
    );
    fixture
        .publish(vec![tool("echo", 2), tool("added", 2)])
        .await;
    fixture.wait_tools(&[("echo", 2), ("added", 2)]).await;
    let second = fixture.registry().await;
    assert!(
        second
            .definitions()
            .iter()
            .find(|tool| tool.id == "mcp__dynamic__echo")
            .unwrap()
            .parameters["properties"]
            .get("value2")
            .is_some()
    );
    let help = second
        .execute(
            "mcpctl",
            json!({"args":["dynamic","echo","--help"]}),
            &fixture.ctx,
        )
        .await
        .unwrap();
    assert!(help.text_content().contains("value2"));
    assert!(!help.text_content().contains("value1"));
    let service = &fixture.app.state.mcp_service;
    let list = service
        .bridge_list_servers(&fixture.ctx.user.id, Some(&fixture.ctx.agent.id))
        .await
        .unwrap();
    assert_eq!(list[0].tool_count, 2);
    let detail = service
        .bridge_server_tools(&fixture.ctx.user.id, None, "dynamic")
        .await
        .unwrap();
    assert_eq!(detail.tools.len(), 2);
    assert!(
        detail
            .tools
            .iter()
            .all(|tool| tool.description == "version 2")
    );
    fixture.publish(vec![]).await;
    fixture.wait_tools(&[]).await;
    assert!(
        fixture
            .app
            .state
            .mcp_manager
            .server_for_tool(&fixture.ctx.user.id, "mcp__dynamic__echo")
            .await
            .is_none()
    );
    assert!(
        first
            .execute("mcp__dynamic__echo", json!({}), &fixture.ctx)
            .await
            .is_err()
    );
    assert!(
        service
            .bridge_call_tool(&fixture.ctx.user.id, None, "dynamic", "echo", json!({}))
            .await
            .is_err()
    );
    assert!(
        service
            .bridge_server_tools(&fixture.ctx.user.id, None, "dynamic")
            .await
            .unwrap()
            .tools
            .is_empty()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn failed_refresh_retains_registration_and_manager_stop_removes_it() {
    let fixture = Fixture::new().await;
    fixture.start().await;
    fixture.publish(vec![tool("echo", 1)]).await;
    fixture.wait_tools(&[("echo", 1)]).await;
    fixture.remote.fail_list.store(true, Ordering::SeqCst);
    fixture.publish(vec![tool("broken", 2)]).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.remote.failed.notified(),
    )
    .await
    .unwrap();
    {
        // Synchronize with the notification's fetch through the same client mutex.
        let connections = fixture.app.state.mcp_manager.connections_mut().await;
        assert!(
            connections
                .get("dynamic")
                .unwrap()
                .client
                .refresh_tools()
                .await
                .is_err()
        );
    }
    fixture.wait_tools(&[("echo", 1)]).await;
    fixture.remote.fail_list.store(false, Ordering::SeqCst);
    fixture.remote.block_next.store(true, Ordering::SeqCst);
    fixture.publish(vec![tool("late", 2)]).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.remote.started.notified(),
    )
    .await
    .unwrap();
    let manager = fixture.app.state.mcp_manager.clone();
    let stop = tokio::spawn(async move {
        manager.stop("dynamic").await.unwrap();
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), stop)
        .await
        .unwrap()
        .unwrap();
    fixture.wait_tools(&[]).await;
    *fixture.remote.tools.write().await = vec![tool("replacement", 3)];
    fixture.start().await;
    fixture.wait_tools(&[("replacement", 3)]).await;
    fixture.finish().await;
}

#[tokio::test]
async fn notification_during_initial_list_is_published_when_registration_attaches() {
    let fixture = Fixture::new().await;
    *fixture.remote.tools.write().await = vec![tool("initial", 1)];
    fixture.remote.block_next.store(true, Ordering::SeqCst);
    let service = fixture.app.state.mcp_service.clone();
    let user_id = fixture.ctx.user.id.clone();
    let start = tokio::spawn(async move {
        service.start(&user_id, "dynamic").await.unwrap();
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.remote.started.notified(),
    )
    .await
    .unwrap();
    fixture.publish(vec![tool("latest", 2)]).await;
    fixture.remote.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), start)
        .await
        .unwrap()
        .unwrap();
    fixture.wait_tools(&[("latest", 2)]).await;
    fixture.finish().await;
}
