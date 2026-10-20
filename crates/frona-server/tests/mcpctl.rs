//! Real sessions register the direct wrapper; HTTP CLI parity uses the same service and MCP transport.
mod helpers;

use frona::{
    core::{Principal, repository::Repository},
    db::repo::generic::SurrealRepo,
    tool::{
        InferenceContext, ToolDefinition,
        mcp::{McpClient, McpConnection, default_client_info, models::*},
        registry::{AgentToolRegistry, ToolFilter},
    },
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use surrealdb::{Surreal, engine::local::Mem};

#[derive(Clone)]
struct EchoServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    calls: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    count: i64,
    #[serde(default)]
    data: std::collections::BTreeMap<String, Value>,
}

#[tool_router]
impl EchoServer {
    #[tool(description = "Echo typed arguments")]
    fn echo(&self, Parameters(args): Parameters<EchoArgs>) -> CallToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if args.text == "fail" {
            CallToolResult::error(vec![ContentBlock::text("MCP failure detail")])
        } else if args.text == "oversized" {
            CallToolResult::success(vec![ContentBlock::text("x".repeat(4 * 1024 * 1024))])
        } else if args.text == "large" {
            CallToolResult::success(vec![ContentBlock::text("é".repeat(64 * 1024))])
        } else {
            CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string(&args).unwrap(),
            )])
        }
    }

    #[tool(description = "Forbidden private tool")]
    fn secret(&self) -> CallToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        CallToolResult::success(vec![ContentBlock::text("secret operation")])
    }

    #[tool(description = "Slow operation")]
    async fn slow(&self) -> CallToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        CallToolResult::success(vec![ContentBlock::text("done")])
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

struct Fixture {
    app: helpers::app_state::Fixture,
    ctx: InferenceContext,
    endpoint: String,
    http: Option<tokio::task::JoinHandle<()>>,
    calls: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
}

impl Fixture {
    async fn new(bridge_mode: bool) -> Self {
        let fixture = Self::default_deny(bridge_mode).await;
        fixture
            .app
            .state
            .policy_service
            .sync_base_policies()
            .await
            .unwrap();
        fixture
    }

    async fn default_deny(bridge_mode: bool) -> Self {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        frona::db::init::setup_schema(&db).await.unwrap();
        let mut app = helpers::app_state::build_configured(&db, |config| {
            config.mcp.bridge_mode = bridge_mode;
        })
        .await;
        let models = helpers::test_model_service_with_group(
            "mock",
            Arc::new(helpers::MockModelProvider::new(vec![])),
            "primary",
            helpers::test_model_group(),
        )
        .await;
        let state = &mut app.state;
        state.chat_service = frona::chat::service::ChatService::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            state.agent_service.clone(),
            models,
            state.storage_service.clone(),
            state.user_service.clone(),
            state.prompts.clone(),
            state.broadcast_service.clone(),
            state.presign_service.clone(),
            state.usage_service.clone(),
        );
        state.harness = Arc::new(frona::agent::harness::Harness::new(
            state.chat_service.clone(),
            state.user_service.clone(),
            state.storage_service.clone(),
            state.agent_service.clone(),
            helpers::test_memory_service(state, &db),
            state.skill_service.clone(),
            state.task_service.clone(),
            state.mcp_service.clone(),
            state.tool_manager.clone(),
            state.policy_service.clone(),
            state.broadcast_service.clone(),
            state.active_sessions.clone(),
            state.execution_registry.clone(),
            state.shutdown_token.clone(),
            state.prompts.clone(),
            state.config.clone(),
            state.usage_service.clone(),
        ));
        let mut ctx = helpers::mock_context();
        ctx.agent.prompt = Some("Use available tools.".into());
        SurrealRepo::<frona::chat::models::Chat>::new(db.clone())
            .create(ctx.chat.as_ref().unwrap())
            .await
            .unwrap();
        app.state.user_service.create(&ctx.user).await.unwrap();
        SurrealRepo::<frona::agent::models::Agent>::new(db.clone())
            .create(&ctx.agent)
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        for handle in ["demo", "private"] {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let server = EchoServer {
                tool_router: EchoServer::tool_router(),
                calls: calls.clone(),
                started: started.clone(),
            };
            tokio::spawn(async move {
                let server = server.serve(server_io).await.unwrap();
                let _ = server.waiting().await;
            });
            let client = McpClient::connect(client_io, default_client_info())
                .await
                .unwrap();
            let cached = client.cached_tools().await;
            let definitions: Vec<_> = cached
                .iter()
                .map(|tool| ToolDefinition {
                    id: format!("mcp__{handle}__{}", tool.name),
                    provider_id: format!("mcp:{handle}"),
                    description: tool.description.clone(),
                    parameters: tool.input_schema.clone(),
                })
                .collect();
            let now = chrono::Utc::now();
            let server = McpServer {
                id: handle.into(),
                user_id: ctx.user.id.clone(),
                handle: frona::core::Handle::try_new(handle).unwrap(),
                display_name: handle.into(),
                description: Some(format!("{handle} server")),
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
                transports: vec![],
                active_transport: "stdio".into(),
                env: Default::default(),
                status: McpServerStatus::Running,
                tool_cache: cached,
                workspace_dir: String::new(),
                installed_at: now,
                last_started_at: Some(now),
                updated_at: now,
            };
            SurrealRepo::<McpServer>::new(db.clone())
                .create(&server)
                .await
                .unwrap();
            app.state.mcp_manager.connections_mut().await.insert(
                handle.into(),
                McpConnection {
                    server_id: handle.into(),
                    handle: server.handle,
                    user_id: ctx.user.id.clone(),
                    client,
                    child: None,
                    port: None,
                    restart_count: 0,
                    log_path: None,
                    token_guard: None,
                },
            );
            app.state
                .mcp_service
                .register_tools(&ctx.user.id, handle, definitions)
                .await;
        }
        Self {
            app,
            ctx,
            endpoint: String::new(),
            http: None,
            calls,
            started,
        }
    }

    async fn start_http(&mut self) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        self.endpoint = format!("http://{}", listener.local_addr().unwrap());
        let router = frona::api::routes::mcp::router().with_state(self.app.state.clone());
        self.http = Some(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
    }

    async fn registry(&self) -> AgentToolRegistry {
        self.registry_with_filters(&[]).await
    }

    async fn registry_with_filters(&self, filters: &[ToolFilter]) -> AgentToolRegistry {
        frona::chat::session::ChatSessionContext::build(
            &self.app.state.harness,
            &self.ctx.user.id,
            self.ctx.chat.clone().unwrap(),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(frona::inference::conversation::DefaultConversationBuilder {
                user_service: self.app.state.user_service.clone(),
                storage_service: self.app.state.storage_service.clone(),
                agent_service: self.app.state.agent_service.clone(),
            }),
            filters,
        )
        .await
        .unwrap()
        .tool_registry
    }

    async fn forbid(&self, id: &str, resource: &str) {
        self.app.state.policy_service.create_policy(&self.ctx.user.id, &format!(
            "@id(\"{id}\") forbid(principal, action, resource == Policy::Tool::{resource});", resource=serde_json::to_string(resource).unwrap()
        )).await.unwrap();
    }

    async fn token(&self) -> String {
        self.app
            .state
            .token_service
            .create_token(
                &self.app.state.keypair_service,
                &self.ctx.user,
                frona::auth::token::service::CreateTokenRequest {
                    token_type: frona::auth::token::models::TokenType::Ephemeral,
                    principal: Principal::agent(&self.ctx.agent.id),
                    ttl_secs: 120,
                    name: String::new(),
                    scopes: vec![],
                    refresh_pair_id: None,
                    extensions: None,
                },
            )
            .await
            .unwrap()
            .jwt
    }

    async fn finish(self) {
        self.app.state.mcp_manager.stop("demo").await.unwrap();
        self.app.state.mcp_manager.stop("private").await.unwrap();
        if let Some(http) = self.http {
            http.abort();
        }
    }
}

#[tokio::test]
async fn wrapper_matches_shared_cli_under_equivalent_credentials_without_shell_access() {
    let mut fixture = Fixture::new(true).await;
    fixture.start_http().await;
    fixture.forbid("deny-shell", "shell").await;
    let registry = fixture.registry().await;
    assert!(!registry.definitions().iter().any(|tool| tool.id == "shell"));
    let offered: Vec<_> = registry
        .definitions()
        .iter()
        .filter(|tool| tool.id == "mcpctl")
        .collect();
    assert_eq!(offered.len(), 1);
    assert!(offered[0].description.contains("No shell permission"));
    assert_eq!(offered[0].parameters["properties"]["args"]["type"], "array");
    let token = fixture.token().await;
    for args in [
        json!([]),
        json!(["list"]),
        json!(["help", "demo"]),
        json!(["demo", "echo", "--help"]),
        json!([
            "demo",
            "echo",
            "--text",
            "space ; $(echo nope)",
            "--enabled",
            "true",
            "--count",
            "7",
            "--data",
            "{\"x\":1}"
        ]),
        json!(["demo", "echo", "--text", "fail"]),
        json!(["demo", "missing"]),
        json!(["demo", "echo"]),
        json!(["demo", "echo", "--text", "large"]),
        json!(["demo", "echo", "--text", "oversized"]),
        json!([".", "--help"]),
        json!(["demo", ".."]),
        json!(["help", "demo", ".."]),
        json!(["unknown", "--help"]),
    ] {
        let cli = frona_cli_mcp::run(
            frona_cli_mcp::BridgeClient::new(fixture.endpoint.clone(), token.clone()),
            std::iter::once("mcpctl".to_string())
                .chain(
                    args.as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string()),
                )
                .collect(),
        )
        .await;
        let output = registry
            .execute("mcpctl", json!({"args":args}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(output.is_success(), cli.success);
        let expected = if cli.stderr.is_empty() {
            cli.stdout
        } else {
            format!("{}stderr:\n{}", cli.stdout, cli.stderr)
        };
        assert_eq!(output.text_content(), expected);
        if expected.contains("space ;") {
            assert!(output.is_success());
            let echoed: Value = serde_json::from_str(output.text_content()).unwrap();
            assert_eq!(echoed["enabled"], true);
            assert_eq!(echoed["count"], 7);
            assert_eq!(echoed["data"], json!({"x":1}));
        }
        assert!(!output.text_content().contains(&token));
        assert!(output.text_content().len() <= 128 * 1024 + 8);
    }
    for arguments in [
        json!({"args":[],"api_url":"https://example.com"}),
        json!({"args":[],"token":"other"}),
        json!({"args":"list"}),
        json!({"args":[1]}),
        json!({"args":vec!["x";257]}),
    ] {
        assert!(
            !registry
                .execute("mcpctl", arguments, &fixture.ctx)
                .await
                .unwrap()
                .is_success()
        );
    }
    fixture.finish().await;
}

#[tokio::test]
async fn wrapper_is_always_offered_while_target_calls_require_permission() {
    let fixture = Fixture::default_deny(true).await;
    assert!(
        fixture.http.is_none(),
        "direct wrapper fixture has no HTTP listener"
    );
    let empty = fixture.registry().await;
    assert!(empty.definitions().iter().any(|tool| tool.id == "mcpctl"));
    let listed = empty
        .execute("mcpctl", json!({"args":["list"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(listed.is_success());
    assert_eq!(listed.text_content(), "No MCP servers available.\n");
    let guessed = empty
        .execute(
            "mcpctl",
            json!({"args":["demo","echo","--text","denied"]}),
            &fixture.ctx,
        )
        .await
        .unwrap();
    assert!(!guessed.is_success());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);

    fixture.app.state.policy_service.create_policy(&fixture.ctx.user.id,
        "@id(\"only-echo\") permit(principal, action == Policy::Action::\"invoke_tool\", resource == Policy::Tool::\"mcp__demo__echo\");"
    ).await.unwrap();
    let registry = fixture.registry().await;
    let mut names = registry
        .definitions()
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["mcp__demo__echo", "mcpctl"]);
    let decision = fixture
        .app
        .state
        .policy_service
        .authorize(
            &fixture.ctx.user.id,
            &fixture.ctx.agent,
            frona::policy::models::PolicyAction::InvokeTool {
                tool_name: "mcpctl".into(),
                tool_group: "mcpctl".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        !decision.allowed,
        "wrapper access must not require its own Cedar grant"
    );
    let output = registry
        .execute(
            "mcpctl",
            json!({"args":["demo","echo","--text","permitted"]}),
            &fixture.ctx,
        )
        .await
        .unwrap();
    assert!(output.is_success(), "{}", output.text_content());
    assert!(output.text_content().contains("permitted"));
    let denied = registry
        .execute("mcpctl", json!({"args":["private","--help"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(!denied.is_success());
    let filtered = fixture
        .registry_with_filters(&[ToolFilter::AllowList(&["complete_task"])])
        .await;
    assert!(
        !filtered
            .definitions()
            .iter()
            .any(|tool| tool.id == "mcpctl")
    );
    fixture
        .app
        .state
        .agent_service
        .delete(&fixture.ctx.user.id, &fixture.ctx.agent.id)
        .await
        .unwrap();
    let stale = registry
        .execute("mcpctl", json!({"args":["list"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(
        !stale.is_success(),
        "direct calls must reload the persisted agent"
    );
    assert!(stale.text_content().contains("Agent not found"));
    fixture.finish().await;
}

#[tokio::test]
async fn current_target_permissions_filter_discovery_help_and_execution() {
    let mut fixture = Fixture::new(true).await;
    fixture.start_http().await;
    fixture.forbid("deny-secret", "mcp__demo__secret").await;
    fixture.forbid("deny-slow", "mcp__demo__slow").await;
    for tool in ["echo", "secret", "slow"] {
        fixture
            .forbid(
                &format!("deny-private-{tool}"),
                &format!("mcp__private__{tool}"),
            )
            .await;
    }
    let mut registry = fixture.registry().await;
    let list = registry
        .execute("mcpctl", json!({"args":["list"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(list.text_content().contains("(1 tools)"));
    assert!(!list.text_content().contains("private"));
    let help = registry
        .execute("mcpctl", json!({"args":["demo","--help"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(help.text_content().contains("echo"));
    assert!(!help.text_content().contains("secret"));
    assert!(!help.text_content().contains("slow"));
    for args in [
        json!(["private", "--help"]),
        json!(["private", "echo", "--text", "no"]),
        json!(["demo", "secret", "--help"]),
        json!(["demo", "slow"]),
    ] {
        assert!(
            !registry
                .execute("mcpctl", json!({"args":args}), &fixture.ctx)
                .await
                .unwrap()
                .is_success()
        );
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let token = fixture.token().await;
    let response = reqwest::Client::new()
        .post(format!(
            "{}/api/mcp/bridge/private/call/secret",
            fixture.endpoint
        ))
        .bearer_auth(&token)
        .json(&json!({"arguments":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let response = reqwest::Client::new()
        .get(format!(
            "{}/api/mcp/bridge/servers/private",
            fixture.endpoint
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    fixture.forbid("revoke-echo", "mcp__demo__echo").await;
    let response = reqwest::Client::new()
        .post(format!(
            "{}/api/mcp/bridge/demo/call/echo",
            fixture.endpoint
        ))
        .bearer_auth(&token)
        .json(&json!({"arguments":{"text":"no"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let empty = registry
        .execute("mcpctl", json!({"args":["list"]}), &fixture.ctx)
        .await
        .unwrap();
    assert_eq!(empty.text_content(), "No MCP servers available.\n");
    let refreshed = fixture.registry().await;
    assert!(
        refreshed
            .definitions()
            .iter()
            .any(|tool| tool.id == "mcpctl")
    );
    let revoked = refreshed
        .execute(
            "mcpctl",
            json!({"args":["demo","echo","--text","denied"]}),
            &fixture.ctx,
        )
        .await
        .unwrap();
    assert!(!revoked.is_success());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    registry.apply_filter(&ToolFilter::AllowList(&[
        "complete_task",
        "fail_task",
        "defer_task",
    ]));
    assert!(
        registry
            .execute("mcpctl", json!({"args":[]}), &fixture.ctx)
            .await
            .is_err()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn wrapper_cancellation_timeout_and_native_mode_are_explicit() {
    let mut fixture = Fixture::new(true).await;
    let registry = fixture.registry().await;
    assert!(registry.definitions().iter().any(|tool| tool.id == "shell"));
    fixture.ctx.cancel_token.cancel();
    let cancelled = registry
        .execute("mcpctl", json!({"args":["demo","slow"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(!cancelled.is_success());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    fixture.ctx.cancel_token = tokio_util::sync::CancellationToken::new();
    let cancel = fixture.ctx.cancel_token.clone();
    let started = fixture.started.clone();
    let cancellation = tokio::spawn(async move {
        started.notified().await;
        cancel.cancel();
    });
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        registry.execute("mcpctl", json!({"args":["demo","slow"]}), &fixture.ctx),
    )
    .await
    .unwrap()
    .unwrap();
    cancellation.await.unwrap();
    assert!(cancelled.text_content().contains("cancelled"));
    fixture.ctx.cancel_token = tokio_util::sync::CancellationToken::new();
    fixture.ctx.agent.sandbox_limits = Some(frona::core::config::SandboxLimits {
        timeout_secs: 1,
        ..Default::default()
    });
    let timed_out = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        registry.execute("mcpctl", json!({"args":["demo","slow"]}), &fixture.ctx),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        timed_out
            .text_content()
            .contains("timed out after 1 seconds")
    );
    fixture.finish().await;
    let fixture = Fixture::new(false).await;
    let registry = fixture.registry().await;
    assert!(!registry.mcp_bridge_mode());
    assert!(
        registry
            .definitions()
            .iter()
            .any(|tool| tool.id == "mcpctl")
    );
    let output = registry
        .execute(
            "mcp__demo__echo",
            json!({"text":"native unchanged"}),
            &fixture.ctx,
        )
        .await
        .unwrap();
    assert!(output.is_success());
    assert!(output.text_content().contains("native unchanged"));
    let wrapped = registry
        .execute(
            "mcpctl",
            json!({"args":["demo","echo","--text","wrapper in native mode"]}),
            &fixture.ctx,
        )
        .await
        .unwrap();
    assert!(wrapped.is_success());
    assert!(wrapped.text_content().contains("wrapper in native mode"));
    fixture.finish().await;
}

#[tokio::test]
async fn wrapper_remains_available_as_servers_and_tools_are_removed() {
    let fixture = Fixture::new(true).await;
    let registry = fixture.registry().await;
    assert_eq!(
        registry
            .definitions()
            .iter()
            .filter(|tool| tool.id == "mcpctl")
            .count(),
        1
    );

    fixture
        .app
        .state
        .mcp_service
        .stop(&fixture.ctx.user.id, "demo")
        .await
        .unwrap();
    let registry = fixture.registry().await;
    assert_eq!(
        registry
            .definitions()
            .iter()
            .filter(|tool| tool.id == "mcpctl")
            .count(),
        1
    );
    assert!(
        !registry
            .definitions()
            .iter()
            .any(|tool| tool.id.starts_with("mcp__demo__"))
    );
    let output = registry
        .execute("mcpctl", json!({"args": ["list"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(output.is_success());
    assert!(output.text_content().contains("private"));
    assert!(!output.text_content().contains("demo"));

    let private_tools = registry
        .definitions()
        .iter()
        .filter(|tool| tool.id.starts_with("mcp__private__"))
        .cloned()
        .collect();
    fixture
        .app
        .state
        .mcp_service
        .register_tools(&fixture.ctx.user.id, "private", vec![])
        .await;
    assert!(
        fixture
            .registry()
            .await
            .definitions()
            .iter()
            .any(|tool| tool.id == "mcpctl")
    );
    fixture
        .app
        .state
        .mcp_service
        .register_tools(&fixture.ctx.user.id, "private", private_tools)
        .await;
    assert_eq!(
        fixture
            .registry()
            .await
            .definitions()
            .iter()
            .filter(|tool| tool.id == "mcpctl")
            .count(),
        1
    );

    fixture
        .app
        .state
        .mcp_service
        .uninstall(&fixture.ctx.user.id, "private")
        .await
        .unwrap();
    let registry = fixture.registry().await;
    assert!(
        !registry
            .definitions()
            .iter()
            .any(|tool| tool.id.starts_with("mcp__"))
    );
    assert_eq!(
        registry
            .definitions()
            .iter()
            .filter(|tool| tool.id == "mcpctl")
            .count(),
        1
    );
    let listed = registry
        .execute("mcpctl", json!({"args":["list"]}), &fixture.ctx)
        .await
        .unwrap();
    assert!(listed.is_success());
    assert_eq!(listed.text_content(), "No MCP servers available.\n");
    fixture.finish().await;
}
