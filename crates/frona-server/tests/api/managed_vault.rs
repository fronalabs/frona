use super::*;
use frona::core::{Principal, error::AppError, repository::Repository};
use frona::credential::vault::models::*;
use frona::tool::{
    AgentTool, InferenceContext,
    cli::{CliTool, CliToolConfig},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

async fn setup() -> (AppState, tempfile::TempDir, InferenceContext, String) {
    let (state, tmp) = test_app_state_with_sandbox(true).await;
    state.vault_service.sync_config_connections().await.unwrap();
    let (token, uid) = register_user(&state, "managed", "managed@example.com", "password123").await;
    let agent = create_agent(&state, &token, "Managed").await;
    let aid = agent["id"].as_str().unwrap();
    let chat = create_chat(&state, &token, aid, None).await;
    let ctx = InferenceContext::new(
        state.user_service.find_by_id(&uid).await.unwrap().unwrap(),
        state.agent_service.find_by_id(aid).await.unwrap().unwrap(),
        state
            .chat_service
            .get_chat(&uid, chat["id"].as_str().unwrap())
            .await
            .unwrap(),
        frona::chat::broadcast::EventSender::noop(),
        CancellationToken::new(),
        CancellationToken::new(),
    );
    (state, tmp, ctx, token)
}

async fn credential(
    state: &AppState,
    id: Option<uuid::Uuid>,
    access: &str,
    account: &str,
) -> frona::credential::managed::Status {
    let vault = state.model_provider_service.store.vault();
    let document = serde_json::to_value(
        frona::credential::managed::integration::openai_codex::Document {
            access_token: access.into(),
            refresh_token: Some("private-refresh-material".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            account_id: Some(account.into()),
            scopes: vec![],
        },
    )
    .unwrap();
    if let Some(id) = id {
        let current = vault.status_by_id(id).await.unwrap().unwrap();
        vault
            .replace_by_id(
                id,
                current.version,
                "openai_codex",
                json!({"name":"managed-test"}),
                document,
            )
            .await
            .unwrap()
    } else {
        vault
            .create("openai_codex", json!({"name":"managed-test"}), document)
            .await
            .unwrap()
    }
}

async fn approve(state: &AppState, token: &str, principal: &Principal, id: &str) -> String {
    let resp = build_app(state.clone())
        .oneshot(auth_post_json(
            "/api/vaults/grants",
            token,
            json!({"principal":principal, "connection_id":"managed", "vault_item_id":id,
            "query":"LOGIN", "target":{"Prefix":{"env_var_prefix":"LOGIN"}}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["id"].as_str().unwrap().to_owned()
}

fn shell(state: &AppState) -> CliTool {
    CliTool::new(
        CliToolConfig {
            name: "shell".into(),
            description: "fixture".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "${command}".into()],
            stdin: None,
            parameters: Default::default(),
            required: vec!["command".into()],
            timeout_secs: Some(10),
            provider: None,
        },
        state.sandbox_manager.clone(),
    )
}

const SHOW_ENV: &str = "printf '%s|%s|%s' \"${LOGIN_ACCESS_TOKEN-unset}\" \"${LOGIN_ACCOUNT_ID-unset}\" \"${LOGIN_REFRESH_TOKEN-unset}\"";

#[tokio::test]
async fn inference_and_granted_commands_share_a_provider_without_implicit_secret_access() {
    use frona::core::config::ModelProviderConfig;
    use frona::inference::{
        credential::runtime::RuntimeCredentials,
        provider::{InferenceCounter, ModelConfig},
    };
    use wiremock::matchers::{header, method, path};
    let (state, _tmp, ctx, token) = setup().await;
    let server = MockServer::start().await;
    for key in ["first-key", "replacement-key"] {
        Mock::given(method("POST")).and(path("/chat/completions"))
            .and(header("authorization", format!("Bearer {key}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"fixture", "object":"chat.completion", "created":0, "model":"fixture",
                "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            }))).expect(1).mount(&server).await;
    }
    let handle = frona::handle!("managed-inference");
    let initial = state
        .model_provider_service
        .store
        .vault()
        .create(
            "static",
            json!({"name":"managed-inference"}),
            json!({"API_KEY":""}),
        )
        .await
        .unwrap();
    let config = ModelProviderConfig {
        credential_id: Some(initial.item_id),
        provider: Some("openai".into()),
        base_url: Some(server.uri()),
        ..Default::default()
    };
    let model = ModelConfig {
        catalog_provider: String::new(),
        provider_handle: handle.clone(),
        model_id: "fixture".into(),
        provider: frona::core::config::ProviderModel::OpenAI {
            api: Some(frona::core::config::OpenAiApi::ChatCompletions),
            params: Default::default(),
        },
        request_settings: Default::default(),
    };
    let runtime = std::sync::Arc::new(RuntimeCredentials::new(
        [
            (handle.clone(), config.clone()),
            (
                frona::handle!("external-inline"),
                ModelProviderConfig {
                    api_key: Some("external-key".into()),
                    credential_id: None,
                    ..config.clone()
                },
            ),
        ]
        .into(),
        state.model_provider_service.store.clone(),
        InferenceCounter::new(state.broadcast_service.clone()),
    ));
    let prepared = runtime.provider(&handle).unwrap();
    let original = &state.model_provider_service;
    let service = frona::inference::provider::service::ModelProviderService::new(
        original.directory.clone(),
        original.config_service.clone(),
        original.store.clone(),
        original.validator.clone(),
        runtime.clone(),
        original.active.clone(),
        Default::default(),
        runtime.providers(),
    );
    let group = service
        .compile(
            model,
            frona::inference::config::RetryConfig {
                max_retries: 0,
                ..Default::default()
            },
            4096,
        )
        .unwrap();
    let usage_context = crate::helpers::test_usage_ctx();
    assert!(
        group
            .inference(frona::inference::ModelRequest {
                system_prompt: "fixture",
                history: vec![],
                tools: vec![],
                usage_service: &state.usage_service,
                usage_context: &usage_context,
                overrides: Default::default(),
            })
            .await
            .is_err(),
        "startup and compilation do not require credentials"
    );
    let tool = shell(&state);
    let mut identity = None;
    for key in ["first-key", "replacement-key"] {
        let vault = state.model_provider_service.store.vault();
        let current = vault.status_by_id(initial.item_id).await.unwrap().unwrap();
        let active = vault
            .replace_by_id(
                initial.item_id,
                current.version,
                "static",
                json!({"name":"managed-inference"}),
                frona::credential::managed::integration::static_secret::document(key.into()),
            )
            .await
            .unwrap();
        if let Some(id) = identity {
            assert_eq!(active.item_id, id);
        }
        assert!(std::sync::Arc::ptr_eq(
            &prepared,
            &runtime.provider(&handle).unwrap()
        ));
        let frona::inference::ModelResponse { usage, .. } = group
            .inference(frona::inference::ModelRequest {
                system_prompt: "fixture",
                history: vec![],
                tools: vec![],
                usage_service: &state.usage_service,
                usage_context: &usage_context,
                overrides: frona::inference::RequestOverrides {
                    max_tokens: Some(16),
                    temperature: None,
                },
            })
            .await
            .unwrap();
        assert_eq!(usage.input_tokens, 1);
        if identity.is_none() {
            let output = tool
                .execute(
                    "shell",
                    json!({"command":"printf '%s' \"${LOGIN_API_KEY-unset}\""}),
                    &ctx,
                )
                .await
                .unwrap();
            assert_eq!(
                output.text_content().trim_end(),
                "unset",
                "model use must not grant secret export"
            );
            approve(
                &state,
                &token,
                &Principal::agent(&ctx.agent.id),
                &active.item_id.to_string(),
            )
            .await;
        }
        let output = tool
            .execute(
                "shell",
                json!({"command":"printf '%s' \"${LOGIN_API_KEY-unset}\""}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(output.text_content().trim_end(), key);
        identity = Some(active.item_id);
    }
    let items = state
        .vault_service
        .search_all(&ctx.user.id, "", 100)
        .await
        .unwrap();
    assert_eq!(
        items.len(),
        1,
        "inline provider credentials must not become vault entries"
    );
    assert!(
        !serde_json::to_string(&items)
            .unwrap()
            .contains("external-key")
    );
}

#[tokio::test]
async fn commands_use_current_approved_maps_and_never_launch_with_failed_bindings() {
    let (state, tmp, ctx, token) = setup().await;
    let first = credential(&state, None, "one", "alice").await;
    let id = first.item_id.to_string();
    let principal = Principal::agent(&ctx.agent.id);
    let tool = shell(&state);
    let run = || tool.execute("shell", json!({"command":SHOW_ENV}), &ctx);
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "unset|unset|unset"
    );

    let items = build_app(state.clone())
        .oneshot(auth_get("/api/vaults/managed/items?q=managed-test", &token))
        .await
        .unwrap();
    let items = body_json(items).await;
    assert_eq!(items[0]["id"], id);
    assert!(!items.to_string().contains("private-refresh"));
    let grant = approve(&state, &token, &principal, &id).await;
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "one|alice|unset"
    );
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "one|alice|unset"
    );
    let changed = credential(&state, Some(first.item_id), "two", "bob").await;
    assert_eq!(changed.item_id, first.item_id);
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "two|bob|unset"
    );

    // Every binding is required, even one unrelated to this command.
    let bad = state
        .vault_service
        .create_binding(
            &ctx.user.id,
            principal.clone(),
            "unrelated",
            "managed",
            &id,
            CredentialTarget::Single {
                env_var: "OTHER".into(),
                field: VaultField::Custom {
                    name: "missing".into(),
                },
            },
            BindingScope::Durable,
            None,
        )
        .await
        .unwrap();
    let marker = tmp.path().join("launched");
    let command = format!("touch '{}'", marker.display());
    assert!(
        tool.execute("shell", json!({"command":command}), &ctx)
            .await
            .is_err()
    );
    assert!(!marker.exists());
    SurrealRepo::<PrincipalCredentialBinding>::new(state.db.clone())
        .delete(&bad.id)
        .await
        .unwrap();
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "two|bob|unset"
    );

    // A normal-vault failure must also block the real launcher.
    let bad = state
        .vault_service
        .create_binding(
            &ctx.user.id,
            principal.clone(),
            "normal-missing",
            "local",
            "missing-item",
            CredentialTarget::Prefix {
                env_var_prefix: "OTHER".into(),
            },
            BindingScope::Chat {
                chat_id: ctx.chat.as_ref().unwrap().id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    assert!(
        tool.execute("shell", json!({"command":command}), &ctx)
            .await
            .is_err()
    );
    assert!(!marker.exists());
    SurrealRepo::<PrincipalCredentialBinding>::new(state.db.clone())
        .delete(&bad.id)
        .await
        .unwrap();

    assert_eq!(
        credential(&state, Some(first.item_id), "three", "carol")
            .await
            .item_id,
        first.item_id
    );
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "three|carol|unset"
    );

    state
        .vault_service
        .revoke_grant(&ctx.user.id, &grant)
        .await
        .unwrap();
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "unset|unset|unset"
    );
    approve(&state, &token, &principal, &id).await;
    state
        .model_provider_service
        .store
        .vault()
        .delete_by_id(
            first.item_id,
            state
                .model_provider_service
                .store
                .vault()
                .status_by_id(first.item_id)
                .await
                .unwrap()
                .unwrap()
                .version,
        )
        .await
        .unwrap();
    let recreated = credential(&state, None, "four", "dave").await;
    assert_ne!(recreated.item_id, first.item_id);
    assert!(
        tool.execute("shell", json!({"command":command}), &ctx)
            .await
            .is_err()
    );
    assert!(!marker.exists());
    state
        .vault_service
        .delete_bindings_for_principal(&ctx.user.id, &principal)
        .await
        .unwrap();
    assert_eq!(
        run().await.unwrap().text_content().trim_end(),
        "unset|unset|unset"
    );
}

// A tiny real stdio peer. It reports only the fixture fields, never host secrets.
const MCP: &str = r#"
import json, os, sys
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req: continue
    method = req.get('method')
    if method == 'initialize':
        result = {'protocolVersion': req['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'vault-fixture', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'credential', 'description': 'Fixture environment', 'inputSchema': {'type': 'object', 'properties': {}}}]}
    elif method == 'tools/call':
        result = {'content': [{'type': 'text', 'text': '|'.join(os.environ.get(k, 'unset') for k in ['LOGIN_ACCESS_TOKEN', 'LOGIN_ACCOUNT_ID', 'LOGIN_REFRESH_TOKEN'])}]}
    else: result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'result': result}), flush=True)
"#;

#[tokio::test]
async fn mcp_process_restart_resolves_current_values_without_mutating_running_process() {
    use frona::tool::mcp::models::*;
    let (state, tmp, ctx, token) = setup().await;
    let first = credential(&state, None, "one", "alice").await;
    let now = chrono::Utc::now();
    let server = McpServer {
        id: "managed-mcp".into(),
        user_id: ctx.user.id.clone(),
        handle: frona::handle!("managed-mcp"),
        display_name: "Managed fixture".into(),
        description: None,
        repository_url: None,
        registry_id: None,
        server_info: None,
        package: McpPackage {
            runtime: McpRuntime::Binary,
            name: "fixture".into(),
            version: "1".into(),
        },
        command: "/usr/bin/python3".into(),
        args: vec!["-u".into(), "-c".into(), MCP.into()],
        env: Default::default(),
        transports: vec![],
        active_transport: "stdio".into(),
        status: McpServerStatus::Installed,
        tool_cache: vec![],
        workspace_dir: tmp.path().join("mcp").to_string_lossy().into_owned(),
        installed_at: now,
        last_started_at: None,
        updated_at: now,
    };
    SurrealRepo::<McpServer>::new(state.db.clone())
        .create(&server)
        .await
        .unwrap();
    let grant = approve(
        &state,
        &token,
        &Principal::mcp_server(&server.id),
        &first.item_id.to_string(),
    )
    .await;
    state
        .mcp_service
        .start(&ctx.user.id, &server.id)
        .await
        .unwrap();
    let value = || async {
        let result = state
            .mcp_manager
            .call(&server.id, "credential", json!({}))
            .await
            .unwrap();
        serde_json::to_value(result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(value().await, "one|alice|unset");
    credential(&state, Some(first.item_id), "two", "bob").await;
    assert_eq!(value().await, "one|alice|unset");
    state
        .mcp_service
        .stop(&ctx.user.id, &server.id)
        .await
        .unwrap();
    state
        .mcp_service
        .start(&ctx.user.id, &server.id)
        .await
        .unwrap();
    assert_eq!(value().await, "two|bob|unset");
    state
        .vault_service
        .revoke_grant(&ctx.user.id, &grant)
        .await
        .unwrap();
    assert_eq!(value().await, "two|bob|unset");
    state
        .mcp_service
        .stop(&ctx.user.id, &server.id)
        .await
        .unwrap();
    state
        .mcp_service
        .start(&ctx.user.id, &server.id)
        .await
        .unwrap();
    assert_eq!(value().await, "unset|unset|unset");
    state
        .mcp_service
        .stop(&ctx.user.id, &server.id)
        .await
        .unwrap();
    let bad = state
        .vault_service
        .create_binding(
            &ctx.user.id,
            Principal::mcp_server(&server.id),
            "missing",
            "managed",
            &first.item_id.to_string(),
            CredentialTarget::Prefix {
                env_var_prefix: "LOGIN".into(),
            },
            BindingScope::Durable,
            None,
        )
        .await
        .unwrap();
    assert!(
        state
            .mcp_service
            .start(&ctx.user.id, &server.id)
            .await
            .is_err()
    );
    assert!(
        state
            .mcp_manager
            .call(&server.id, "credential", json!({}))
            .await
            .is_err()
    );
    SurrealRepo::<PrincipalCredentialBinding>::new(state.db.clone())
        .delete(&bad.id)
        .await
        .unwrap();
}

struct CaptureFactory(Arc<Mutex<Vec<Value>>>);
struct CaptureAdapter;
impl frona::chat::channel::ChannelFactory for CaptureFactory {
    fn manifest(&self) -> frona::chat::channel::ChannelManifest {
        frona::chat::channel::ChannelManifest {
            id: "managed-fixture".into(),
            display_name: "Fixture".into(),
            description: String::new(),
            config_fields: vec![],
            webhook_url_visible: false,
            setup_instructions: None,
            external_links: vec![],
        }
    }
    fn create(
        &self,
        config: Value,
    ) -> Result<Box<dyn frona::chat::channel::ChannelAdapter>, AppError> {
        self.0.lock().unwrap().push(config);
        Ok(Box::new(CaptureAdapter))
    }
}
#[async_trait::async_trait]
impl frona::chat::channel::ChannelAdapter for CaptureAdapter {
    async fn on_connect(&self, ctx: &frona::chat::channel::ChannelCtx) -> Result<(), AppError> {
        ctx.signals.connected();
        Ok(())
    }
    async fn on_disconnect(&self, _: &frona::chat::channel::ChannelCtx) -> Result<(), AppError> {
        Ok(())
    }
    async fn on_send(
        &self,
        _: &frona::chat::message::models::Message,
        _: &[frona::inference::tool_call::ToolCall],
        _: &frona::chat::models::Chat,
        _: &frona::chat::channel::ChannelCtx,
    ) -> Result<(), frona::chat::channel::ChannelError> {
        Ok(())
    }
}

#[tokio::test]
async fn channel_connection_restart_resolves_current_credentials_before_adapter_creation() {
    use frona::chat::channel::{Channel, ChannelStatus};
    use frona::core::supervisor::Supervisor;
    let (state, _tmp, ctx, token) = setup().await;
    let first = credential(&state, None, "one", "alice").await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    state
        .channel_registry
        .register_factory(Arc::new(CaptureFactory(captured.clone())));
    let space = create_space(&state, &token, "Managed").await;
    let now = chrono::Utc::now();
    let channel = Channel {
        id: "managed-channel".into(),
        user_id: ctx.user.id.clone(),
        handle: frona::handle!("managed-channel"),
        space_id: space["id"].as_str().unwrap().into(),
        provider: "managed-fixture".into(),
        agent_id: ctx.agent.id.clone(),
        config: Default::default(),
        dispatch_mode: Default::default(),
        status: ChannelStatus::Disconnected,
        enabled: true,
        error_message: None,
        last_started_at: None,
        user_address: None,
        setup: None,
        created_at: now,
        updated_at: now,
        webhook_url: None,
    };
    SurrealRepo::<Channel>::new(state.db.clone())
        .create(&channel)
        .await
        .unwrap();
    let grant = approve(
        &state,
        &token,
        &Principal::channel(&channel.id),
        &first.item_id.to_string(),
    )
    .await;
    state.channel_supervisor.start(&channel.id).await.unwrap();
    wait_channel(&state, &channel.id, ChannelStatus::Connected).await;
    assert_eq!(captured.lock().unwrap()[0]["LOGIN_ACCESS_TOKEN"], "one");
    assert!(
        !captured.lock().unwrap()[0]
            .to_string()
            .contains("private-refresh")
    );
    credential(&state, Some(first.item_id), "two", "bob").await;
    assert_eq!(captured.lock().unwrap().len(), 1);
    state.channel_supervisor.stop(&channel.id).await.unwrap();
    wait_channel(&state, &channel.id, ChannelStatus::Disconnected).await;
    state.channel_supervisor.start(&channel.id).await.unwrap();
    wait_channel(&state, &channel.id, ChannelStatus::Connected).await;
    assert_eq!(captured.lock().unwrap()[1]["LOGIN_ACCESS_TOKEN"], "two");
    state.channel_supervisor.stop(&channel.id).await.unwrap();
    wait_channel(&state, &channel.id, ChannelStatus::Disconnected).await;
    state
        .vault_service
        .revoke_grant(&ctx.user.id, &grant)
        .await
        .unwrap();
    state.channel_supervisor.start(&channel.id).await.unwrap();
    wait_channel(&state, &channel.id, ChannelStatus::Connected).await;
    assert_eq!(captured.lock().unwrap()[2], json!({}));
    state.channel_supervisor.stop(&channel.id).await.unwrap();
    wait_channel(&state, &channel.id, ChannelStatus::Disconnected).await;
    state
        .vault_service
        .create_binding(
            &ctx.user.id,
            Principal::channel(&channel.id),
            "missing",
            "managed",
            &first.item_id.to_string(),
            CredentialTarget::Prefix {
                env_var_prefix: "LOGIN".into(),
            },
            BindingScope::Durable,
            None,
        )
        .await
        .unwrap();
    state.channel_supervisor.start(&channel.id).await.unwrap();
    wait_channel(&state, &channel.id, ChannelStatus::Failed).await;
    assert_eq!(
        captured.lock().unwrap().len(),
        3,
        "failed binding must prevent adapter creation"
    );
    state.channel_supervisor.stop(&channel.id).await.unwrap();
}

async fn wait_channel(state: &AppState, id: &str, expected: frona::chat::channel::ChannelStatus) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if state.channel_service.find_by_id(id).await.unwrap().status == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn personal_managed_connections_are_isolated_and_delete_requires_empty_storage() {
    let (state, _tmp, ctx, token) = setup().await;
    let user_id = ctx.user.id.clone();
    let mut ids = Vec::new();
    for name in ["First vault", "Second vault"] {
        let response = build_app(state.clone())
            .oneshot(auth_post_json(
                "/api/vaults",
                &token,
                json!({"name":name,"provider":"managed","config":{"type":"Managed"}}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let connection = body_json(response).await;
        assert_eq!(connection["system_managed"], false);
        ids.push(connection["id"].as_str().unwrap().to_owned());
    }
    let listed = state
        .vault_service
        .list_connections(&user_id)
        .await
        .unwrap();
    assert_eq!(
        listed
            .iter()
            .filter(|c| c.system_managed && c.provider == VaultProviderType::Managed)
            .count(),
        1
    );
    let vault = frona::credential::managed::ManagedVault::new(
        Arc::new(frona::db::repo::managed_vault::SurrealManagedVaultRepo::new(state.db.clone())),
        &state.config_service.active().auth.encryption_secret,
        ids[0].clone(),
    );
    let entry = vault
        .create(
            "static",
            json!({"name":"key"}),
            json!({"API_KEY":"private"}),
        )
        .await
        .unwrap();
    assert_eq!(
        state
            .vault_service
            .search_items(&user_id, &ids[0], "", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        state
            .vault_service
            .search_items(&user_id, &ids[1], "", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        state
            .model_provider_service
            .store
            .vault()
            .status_by_id(entry.item_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        state
            .vault_service
            .delete_connection(&user_id, &ids[0])
            .await
            .is_err()
    );
    assert!(vault.status_by_id(entry.item_id).await.unwrap().is_some());
    let response = build_app(state.clone())
        .oneshot(auth_delete(
            &format!("/api/vaults/{}/items/{}", ids[0], entry.item_id),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    state
        .vault_service
        .delete_connection(&user_id, &ids[0])
        .await
        .unwrap();
    assert!(
        vault
            .create("static", json!({}), json!({"API_KEY":"late"}))
            .await
            .is_err()
    );
    assert!(
        state
            .vault_service
            .search_items(&user_id, &ids[1], "", 10)
            .await
            .unwrap()
            .is_empty()
    );
}
