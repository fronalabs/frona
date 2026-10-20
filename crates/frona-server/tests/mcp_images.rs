//! Real MCP results reach the native provider through the production tool loop.
mod helpers;

use std::sync::Arc;

use base64::Engine;
use frona::{
    core::{Handle, error::AppError, repository::Repository},
    db::repo::generic::SurrealRepo,
    inference::{
        ModelGroup,
        config::ModelRegistryConfig,
        credential::runtime::RuntimeCredentials,
        provider::InferenceCounter,
        tool_call::ToolCall,
        tool_loop::{ToolLoopOutcome, run_tool_loop},
    },
    tool::{
        AgentTool, InferenceContext, ToolDefinition,
        mcp::{McpClient, McpConnection, default_client_info, mcp_tool::McpTool},
        registry::AgentToolRegistry,
    },
};
use rig_core::completion::Message as RigMessage;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::router::tool::ToolRouter,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde_json::{Value, json};
use surrealdb::{Surreal, engine::local::Mem};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[derive(Clone)]
struct ImageServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    result: CallToolResult,
}

#[tool_router]
impl ImageServer {
    fn new(result: CallToolResult) -> Self {
        Self {
            tool_router: Self::tool_router(),
            result,
        }
    }

    #[tool(description = "Capture an image.")]
    fn capture(&self) -> Result<CallToolResult, McpError> {
        Ok(self.result.clone())
    }
}

#[tool_handler]
impl ServerHandler for ImageServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

fn png() -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 1)
        .write_to(&mut buffer, image::ImageFormat::Png)
        .unwrap();
    buffer.into_inner()
}

fn image_block() -> ContentBlock {
    ContentBlock::image(
        base64::engine::general_purpose::STANDARD.encode(png()),
        "image/png",
    )
}

struct McpFixture {
    app: helpers::app_state::Fixture,
    ctx: InferenceContext,
    tool: Arc<McpTool>,
}

async fn fixture(result: CallToolResult) -> McpFixture {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let app = helpers::app_state::build(&db).await;
    let ctx = helpers::mock_context();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = ImageServer::new(result).serve(server_io).await.unwrap();
        server.waiting().await.unwrap();
    });
    let client = McpClient::connect(client_io, default_client_info())
        .await
        .unwrap();
    let cached = client.cached_tools().await;
    let definitions: Vec<_> = cached
        .into_iter()
        .map(|tool| ToolDefinition {
            id: format!("mcp__images__{}", tool.name),
            provider_id: "mcp__images".into(),
            description: tool.description,
            parameters: tool.input_schema,
        })
        .collect();
    app.state.mcp_manager.connections_mut().await.insert(
        "image-server".into(),
        McpConnection {
            server_id: "image-server".into(),
            handle: Handle::try_new("images").unwrap(),
            user_id: ctx.user.id.clone(),
            client,
            child: None,
            port: None,
            restart_count: 0,
            log_path: None,
            token_guard: None,
        },
    );
    let tool = Arc::new(McpTool::new(
        app.state.mcp_manager.clone(),
        "images",
        definitions,
    ));
    McpFixture { app, ctx, tool }
}

fn model_group(state: &frona::core::state::AppState, endpoint: &str) -> ModelGroup {
    let config: frona::core::config::Config = serde_json::from_value(json!({
        "providers": {"fixture": {"provider": "google", "api_key": "fixture-key", "base_url": endpoint}},
        "models": {"test": {"provider": "fixture", "model": "test-model", "max_tokens": 4096, "retry": {"max_retries": 0}}}
    })).unwrap();
    let registry = ModelRegistryConfig {
        providers: config.providers,
        models: config.models,
        skip_auto_discover: true,
    };
    let runtime = RuntimeCredentials::new(
        registry.providers.clone(),
        state.model_provider_service.store.clone(),
        InferenceCounter::new(state.broadcast_service.clone()),
    );
    registry
        .parse_model_groups(&state.config.inference, Arc::new(runtime.providers()))
        .unwrap()
        .remove("test")
        .unwrap()
}

struct ModelRun {
    outcome: Result<ToolLoopOutcome, AppError>,
    requests: Vec<wiremock::Request>,
    calls: Vec<ToolCall>,
}

async fn run_model(fixture: &McpFixture, reject_images: bool) -> ModelRun {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(move |request: &wiremock::Request| {
        let body: Value = request.body_json().unwrap();
        let after_tool = body["contents"].as_array().unwrap().iter().any(|message| {
            message["parts"].as_array().unwrap().iter().any(|part| part.get("functionResponse").is_some())
        });
        if after_tool && reject_images {
            return ResponseTemplate::new(400).set_body_json(json!({"error": {
                "code": 400, "status": "INVALID_ARGUMENT", "message": "This model does not support image input"
            }}));
        }
        let parts = if after_tool {
            json!([{"text": "I can see the image."}])
        } else {
            json!([{"functionCall": {"name": "mcp__images__capture", "args": {}}}])
        };
        let body = json!({
            "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3, "totalTokenCount": 13}
        });
        ResponseTemplate::new(200).set_body_raw(format!("data: {body}\n\n"), "text/event-stream")
    }).mount(&server).await;
    let state = &fixture.app.state;
    let ctx = &fixture.ctx;
    SurrealRepo::new(state.db.clone())
        .create(&ctx.user)
        .await
        .unwrap();
    SurrealRepo::new(state.db.clone())
        .create(&ctx.agent)
        .await
        .unwrap();
    let chat = ctx.chat.as_ref().unwrap();
    SurrealRepo::new(state.db.clone())
        .create(chat)
        .await
        .unwrap();
    let message = state
        .chat_service
        .create_executing_agent_message(&chat.id, &ctx.agent.id)
        .await
        .unwrap();
    let mut registry = AgentToolRegistry::empty();
    registry.register(fixture.tool.clone());
    let (events, _sse, _broadcast) = helpers::test_event_sender().await;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run_tool_loop(
            &model_group(state, &server.uri()),
            "Describe the image.",
            vec![RigMessage::user("Capture an image.")],
            &registry,
            events,
            CancellationToken::new(),
            ctx,
            &state.usage_service,
            &state.chat_service,
            &message.id,
            None,
        ),
    )
    .await
    .expect("model loop should settle");
    ModelRun {
        outcome,
        requests: server.received_requests().await.unwrap(),
        calls: state
            .chat_service
            .get_tool_calls_by_message(&message.id)
            .await
            .unwrap(),
    }
}

fn assert_image_request(run: &ModelRun, expected_text: &str) {
    assert_eq!(run.requests.len(), 2);
    let body: Value = run.requests[1].body_json().unwrap();
    let parts: Vec<_> = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .collect();
    let images: Vec<_> = parts
        .iter()
        .filter_map(|part| part.get("inlineData"))
        .collect();
    assert_eq!(images.len(), 1, "{body}");
    assert_eq!(images[0]["mimeType"], "image/png");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(images[0]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, png());
    let result = parts
        .iter()
        .find_map(|part| part.get("functionResponse"))
        .unwrap();
    assert!(
        result["response"]
            .to_string()
            .contains(&expected_text.replace('\n', "\\n")),
        "{result}"
    );
    assert_eq!(run.calls.len(), 1);
    assert_eq!(run.calls[0].result, expected_text);
}

#[tokio::test]
async fn mixed_mcp_images_reach_vision_requests_including_tool_errors() {
    for is_error in [false, true] {
        let mut result = CallToolResult::success(vec![
            ContentBlock::text("before"),
            image_block(),
            ContentBlock::text("after"),
        ]);
        result.is_error = Some(is_error);
        let fixture = fixture(result).await;
        let output = fixture
            .tool
            .execute("mcp__images__capture", json!({}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(output.is_success(), !is_error);
        assert_eq!(output.images().len(), 1);
        let run = run_model(&fixture, false).await;
        assert_image_request(&run, "before\nafter");
        assert_eq!(run.calls[0].success, !is_error);
        assert!(
            matches!(run.outcome, Ok(ToolLoopOutcome::Completed { .. })),
            "{:?}",
            run.outcome
        );
        fixture
            .app
            .state
            .mcp_manager
            .stop("image-server")
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn image_only_results_reach_vision_and_nonvision_rejections_are_explicit() {
    for reject_images in [false, true] {
        let fixture = fixture(CallToolResult::success(vec![image_block()])).await;
        let run = run_model(&fixture, reject_images).await;
        assert_image_request(&run, "");
        assert!(run.calls[0].success);
        if reject_images {
            let error = run.outcome.unwrap_err();
            assert!(
                format!("{error:?}").contains("does not support image input"),
                "{error:?}"
            );
        } else {
            assert!(
                matches!(run.outcome, Ok(ToolLoopOutcome::Completed { .. })),
                "{:?}",
                run.outcome
            );
        }
        fixture
            .app
            .state
            .mcp_manager
            .stop("image-server")
            .await
            .unwrap();
    }
}
