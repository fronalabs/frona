//! Copilot inference transport. Managed credentials own token exchanges.

use crate::core::config::{ApiSurface, ModelProviderConfig};
use crate::credential::managed::integration::copilot::CopilotIntegration;
use crate::inference::credential::store::CredentialMethod;
use crate::inference::{
    credential::runtime::CredentialMethodDriver,
    error::InferenceError,
    protocol::http::WireClient,
    provider::{
        CredentialValidation, CredentialValidationError, InferenceCounter, InferenceOutput,
        ModelConfig, ModelListError, ModelProvider, ProviderModelList, RigProvider, StreamToken,
        platform::ResolvedConnection,
    },
};
use chrono::{DateTime, Utc};
use rig_core::{
    client::ModelListingClient,
    completion::{Message, ToolDefinition},
    providers::copilot,
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

pub const ENDPOINT: &str = "https://api.githubcopilot.com";
const PROTOCOLS: &[ApiSurface] = &[ApiSurface::Completions, ApiSurface::Responses];

pub fn protocol(model: &str) -> ApiSurface {
    if model.to_ascii_lowercase().contains("codex") {
        ApiSurface::Responses
    } else {
        ApiSurface::Completions
    }
}

#[derive(Default, Clone)]
pub struct Driver {
    #[cfg(test)]
    api_endpoint: Option<String>,
}

impl Driver {
    fn api_endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some(endpoint) = &self.api_endpoint {
            return endpoint;
        }
        ENDPOINT
    }
}

pub(crate) fn current_provider(
    endpoint: &str,
    secret: &crate::credential::managed::integration::ErasedSecret,
    counter: &InferenceCounter,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    let credentials = secret
        .credentials::<copilot::CopilotAuth>()
        .map_err(|error| InferenceError::ConfigError(error.to_string()))?;
    let client =
        copilot::Client::builder()
            .api_key(credentials.clone())
            .allow_device_flow(false)
            .base_url(endpoint)
            .http_client(WireClient::without_redirects().map_err(|_| {
                InferenceError::ConfigError("Cannot build Copilot HTTP client".into())
            })?)
            .build()
            .map_err(|_| InferenceError::ConfigError("Cannot build Copilot client".into()))?;
    let check = client.clone();
    Ok(Arc::new(
        RigProvider::new(client, counter.clone())
            .with_wire_transport()
            .with_live_check(move || {
                let client = check.clone();
                async move {
                    client
                        .list_models()
                        .await
                        .map_err(CredentialValidationError::from)
                }
            }),
    ))
}

#[async_trait::async_trait]
impl CredentialMethodDriver for Driver {
    fn method(&self) -> CredentialMethod {
        CredentialMethod::Oauth
    }

    fn accepts(&self, connection: &ResolvedConnection) -> bool {
        connection.brand == "github-copilot"
            && connection.effective_base_url.as_deref() == Some(ENDPOINT)
    }

    fn protocols(&self) -> &[ApiSurface] {
        PROTOCOLS
    }

    fn build(
        &self,
        connection: &ResolvedConnection,
        _: &ModelProviderConfig,
        secret: &crate::credential::managed::integration::ErasedSecret,
        counter: &InferenceCounter,
    ) -> Result<Arc<dyn ModelProvider>, InferenceError> {
        if !self.accepts(connection) {
            return Err(InferenceError::ConfigError(
                "Copilot OAuth requires the trusted default endpoint".into(),
            ));
        }
        current_provider(self.api_endpoint(), secret, counter)
    }
}

/// YAML/database API keys contain a supplied GitHub bootstrap token. The short
/// chat token is a derived, expiring client cache, never an operator-home file.
pub struct SuppliedTokenProvider {
    driver: CopilotIntegration,
    github_token: String,
    endpoint: String,
    counter: InferenceCounter,
    cache: Mutex<Option<CachedChatProvider>>,
}

struct CachedChatProvider {
    expires_at: DateTime<Utc>,
    provider: Arc<dyn ModelProvider>,
}

impl SuppliedTokenProvider {
    pub fn new(
        config: &ModelProviderConfig,
        endpoint: &str,
        counter: InferenceCounter,
    ) -> Result<Self, InferenceError> {
        Ok(Self {
            driver: CopilotIntegration::default(),
            github_token: config
                .api_key
                .clone()
                .filter(|key| !key.is_empty())
                .ok_or_else(|| {
                    InferenceError::ConfigError("Copilot requires a GitHub token or login".into())
                })?,
            endpoint: endpoint.into(),
            counter,
            cache: Mutex::new(None),
        })
    }

    async fn provider(&self) -> Result<Arc<dyn ModelProvider>, CredentialValidationError> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.expires_at > Utc::now() + chrono::Duration::seconds(60))
        {
            return Ok(cached.provider.clone());
        }
        let resolved = self
            .driver
            .exchange(&self.github_token)
            .await
            .map_err(|_| {
                CredentialValidationError::Failed("Copilot token exchange failed".into())
            })?;
        let expires = resolved
            .expires_at
            .ok_or_else(|| CredentialValidationError::Failed("missing token expiry".into()))?;
        let provider = current_provider(&self.endpoint, &resolved.erase(), &self.counter)
            .map_err(|_| CredentialValidationError::Failed("Invalid Copilot client".into()))?;
        *cache = Some(CachedChatProvider {
            expires_at: expires,
            provider: provider.clone(),
        });
        Ok(provider)
    }

    async fn inference_provider(&self) -> Result<Arc<dyn ModelProvider>, InferenceError> {
        self.provider()
            .await
            .map_err(|error| InferenceError::InferenceFailed(error.to_string()))
    }
}

#[async_trait::async_trait]
impl ModelProvider for SuppliedTokenProvider {
    async fn validate_credentials(
        &self,
    ) -> Result<CredentialValidation, CredentialValidationError> {
        self.provider().await?.validate_credentials().await
    }

    async fn list_models(&self) -> Result<ProviderModelList, ModelListError> {
        self.provider()
            .await
            .map_err(|error| ModelListError(error.to_string()))?
            .list_models()
            .await
    }

    async fn inference(
        &self,
        model: &ModelConfig,
        system: &str,
        history: Vec<Message>,
        tools: Vec<ToolDefinition>,
        max: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        self.inference_provider()
            .await?
            .inference(model, system, history, tools, max, temperature)
            .await
    }

    async fn stream_inference(
        &self,
        model: &ModelConfig,
        system: &str,
        history: Vec<Message>,
        tools: Vec<ToolDefinition>,
        tx: mpsc::Sender<StreamToken>,
        max: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        self.inference_provider()
            .await?
            .stream_inference(model, system, history, tools, tx, max, temperature)
            .await
    }

    async fn structured_inference(
        &self,
        model: &ModelConfig,
        system: &str,
        history: Vec<Message>,
        schema: serde_json::Value,
        max: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        self.inference_provider()
            .await?
            .structured_inference(model, system, history, schema, max, temperature)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::managed::Candidate;
    use crate::inference::{config::ModelRegistryConfig, provider::platform::ProviderPlatform};
    use crate::{
        chat::broadcast::BroadcastService,
        core::{
            Handle,
            config::{InferenceConfig, ModelGroupConfig},
        },
    };
    use rig_core::completion::AssistantContent;
    use serde_json::{Value, json};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn counter() -> InferenceCounter {
        InferenceCounter::new(BroadcastService::new())
    }

    fn integration(server: &MockServer) -> CopilotIntegration {
        CopilotIntegration {
            exchange_url: format!("{}/exchange", server.uri()),
            device_url: format!("{}/device", server.uri()),
            token_url: format!("{}/token", server.uri()),
            ..Default::default()
        }
    }

    fn driver(server: &MockServer) -> Driver {
        Driver {
            api_endpoint: Some(server.uri()),
            ..Default::default()
        }
    }

    fn config() -> ModelProviderConfig {
        ModelProviderConfig {
            provider: Some("github-copilot".into()),
            ..Default::default()
        }
    }

    fn connection() -> ResolvedConnection {
        ProviderPlatform::resolve(&Handle::const_validated("account"), &config()).unwrap()
    }

    fn token() -> Candidate {
        Candidate::Secret(
            serde_json::to_value(crate::credential::managed::integration::copilot::Document {
                github_token: "fixture-github-token".into(),
            })
            .unwrap(),
        )
    }

    fn model(id: &str) -> ModelConfig {
        let group:ModelGroupConfig=serde_json::from_value(json!({"provider":"account","model":id,"api":protocol(id),"max_tokens":123,"temperature":0.3,"extra_params":{"fixture":{"nullable":null,"array":[1,true]}}})).unwrap();
        ModelRegistryConfig {
            providers: [(Handle::const_validated("account"), config())].into(),
            models: [("primary".into(), group)].into(),
            skip_auto_discover: true,
        }
        .parse_model_groups(&InferenceConfig::default(), Default::default())
        .unwrap()["primary"]
            .main
            .clone()
    }

    fn response(responses: bool, name: &str) -> Value {
        if responses {
            json!({"id":"response1","object":"response","created_at":1,"status":"completed","model":"fixture-codex","output":[{"type":"function_call","id":"fc1","call_id":"call1","name":name,"arguments":"{\"ok\":true}","status":"completed"}],"tools":[],"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}})
        } else {
            json!({"id":"chat1","object":"chat.completion","created":1,"model":"fixture-chat","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call1","type":"function","function":{"name":name,"arguments":"{\"ok\":true}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}})
        }
    }

    fn tool_history() -> Vec<Message> {
        vec![
            Message::user("hello"),
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::tool_call(
                    "previous-call",
                    "lookup",
                    json!({}),
                )],
            },
            Message::User {
                content: vec![
                    rig_core::completion::message::UserContent::tool_result_from_wire(
                        "previous-call",
                        "lookup",
                        vec![rig_core::completion::message::ToolResultContent::text(
                            "external-result",
                        )],
                    ),
                ],
            },
        ]
    }

    fn stream(responses: bool, name: &str) -> String {
        let events = if responses {
            vec![
                json!({"type":"response.output_item.added","output_index":0,"sequence_number":1,"item":{"type":"function_call","id":"fc1","call_id":"call1","name":name,"arguments":"","status":"in_progress"}}),
                json!({"type":"response.function_call_arguments.delta","output_index":0,"sequence_number":2,"delta":"{\"ok\":true}"}),
                json!({"type":"response.output_item.done","output_index":0,"sequence_number":3,"item":{"type":"function_call","id":"fc1","call_id":"call1","name":name,"arguments":"{\"ok\":true}","status":"completed"}}),
                json!({"type":"response.completed","sequence_number":4,"response":response(true,name)}),
            ]
        } else {
            vec![
                json!({"id":"chat1","object":"chat.completion.chunk","created":1,"model":"fixture-chat","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call1","type":"function","function":{"name":name,"arguments":"{\"ok\":true}"}}]},"finish_reason":null}]}),
                json!({"id":"chat1","object":"chat.completion.chunk","created":1,"model":"fixture-chat","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}),
            ]
        };
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n"
    }

    #[tokio::test]
    async fn both_rig_routes_preserve_streams_tools_structured_calls_and_custom_parameters() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &wiremock::Request| {
                let responses = request.url.path() == "/responses";
                let body: Value = request.body_json().unwrap();
                let name = if responses {
                    body["tools"][0]["name"].as_str()
                } else {
                    body["tools"][0]["function"]["name"].as_str()
                }
                .unwrap_or("lookup");
                if body["stream"] == true {
                    ResponseTemplate::new(200)
                        .set_body_raw(stream(responses, name), "text/event-stream")
                } else {
                    ResponseTemplate::new(200).set_body_json(response(responses, name))
                }
            })
            .mount(&server)
            .await;
        let provider = current_provider(
            &server.uri(),
            &crate::credential::managed::integration::ResolvedSecret {
                credentials: copilot::CopilotAuth::ApiKey("fixture-chat-token".into()),
                expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                cache: crate::credential::managed::integration::CachePolicy::NoCache,
            }
            .erase(),
            &counter(),
        )
        .unwrap();
        for id in ["fixture-chat", "fixture-codex"] {
            let model = model(id);
            for streaming in [false, true] {
                let tools = vec![ToolDefinition {
                    name: "lookup".into(),
                    description: "Lookup".into(),
                    parameters: json!({"type":"object","properties":{}}),
                }];
                let output = if streaming {
                    let (tx, _rx) = mpsc::channel(16);
                    provider
                        .stream_inference(&model, "system", tool_history(), tools, tx, None, None)
                        .await
                } else {
                    provider
                        .inference(&model, "system", tool_history(), tools, None, None)
                        .await
                }
                .unwrap();
                assert_eq!(output.usage.input_tokens, 2);
                assert_eq!(output.usage.output_tokens, 3);
                assert!(output.content.iter().any(|content|matches!(content,AssistantContent::ToolCall(call) if call.function.name=="lookup" && call.function.arguments==json!({"ok":true}))));
            }
            assert_eq!(provider.structured_inference(&model,"system",vec![Message::user("hello")],json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}),None,None).await.unwrap(),json!({"ok":true}));
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 6);
        for request in requests {
            assert_eq!(
                request.headers["authorization"],
                "Bearer fixture-chat-token"
            );
            assert_eq!(request.headers["copilot-integration-id"], "vscode-chat");
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["fixture"], json!({"nullable":null,"array":[1,true]}));
            assert_eq!(body["temperature"], 0.3);
            let structured = body["tools"][0]["name"] == "submit"
                || body["tools"][0]["function"]["name"] == "submit";
            if !structured {
                let history = if request.url.path() == "/responses" {
                    &body["input"]
                } else {
                    &body["messages"]
                };
                assert!(history.to_string().contains("external-result"));
                assert!(history.to_string().contains("previous-call"));
            }
            assert_eq!(
                body[if request.url.path() == "/responses" {
                    "max_output_tokens"
                } else {
                    "max_completion_tokens"
                }],
                123
            );
        }
    }

    #[tokio::test]
    async fn managed_bootstrap_token_returns_only_cached_chat_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/exchange")).and(header("authorization", "token fixture-github-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":"fixture-chat-token","expires_at":(Utc::now()+chrono::Duration::hours(1)).timestamp()}))).expect(1).mount(&server).await;
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store = crate::credential::managed::test_support::credentials(db, "key")
            .await
            .with_integrations(
                crate::credential::managed::integration::register([(
                    "copilot".into(),
                    Arc::new(integration(&server))
                        as Arc<dyn crate::credential::managed::integration::RegisteredIntegration>,
                )])
                .unwrap(),
            );
        let handle = crate::core::Handle::const_validated("github-copilot");
        let bootstrap = token();
        let p = store
            .stage(
                &handle,
                CredentialMethod::ApiKey,
                crate::inference::provider::validation::binding(
                    &connection(),
                    "database",
                    &config(),
                ),
                &bootstrap,
            )
            .await
            .unwrap();
        let active = store.promote(p.validation_id, None).await.unwrap();
        for _ in 0..2 {
            let (_, resolved) = store.resolve_credential(active.item_id).await.unwrap();
            assert_eq!(
                resolved.to_env().unwrap()["ACCESS_TOKEN"],
                "fixture-chat-token"
            );
            assert!(
                !resolved
                    .to_env()
                    .unwrap()
                    .values()
                    .any(|v| v == "fixture-github-token")
            );
        }
        assert_eq!(
            store
                .active_id(Some(active.item_id))
                .await
                .unwrap()
                .unwrap()
                .1,
            match bootstrap {
                Candidate::Secret(doc) => doc,
                _ => unreachable!(),
            }
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn token_exchange_refresh_and_live_listing_use_actual_http_without_token_files() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/exchange")).and(header("authorization","token fixture-github-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":"fixture-chat-token","expires_at":(Utc::now()+chrono::Duration::hours(1)).timestamp()}))).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer fixture-chat-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"id":"fixture-chat","name":"Fixture"}]})),
            )
            .mount(&server)
            .await;
        let driver = driver(&server);
        let payload = integration(&server)
            .exchange("fixture-github-token")
            .await
            .unwrap();
        let provider = driver
            .build(&connection(), &config(), &payload.erase(), &counter())
            .unwrap();
        assert!(matches!(
            provider.validate_credentials().await.unwrap().models,
            Some(ProviderModelList::Listed { .. })
        ));
        assert!(matches!(
            provider.list_models().await.unwrap(),
            ProviderModelList::Listed { .. }
        ));
        let refreshed = integration(&server)
            .exchange("fixture-github-token")
            .await
            .unwrap();
        assert_eq!(
            refreshed.to_env().unwrap()["ACCESS_TOKEN"],
            "fixture-chat-token"
        );
        assert!(!refreshed.to_env().unwrap().contains_key("REFRESH_TOKEN"));
        let directory = tempfile::tempdir().unwrap();
        let client = copilot::Client::builder()
            .api_key(copilot::CopilotAuth::ApiKey("fixture-chat-token".into()))
            .allow_device_flow(false)
            .token_dir(directory.path())
            .base_url(server.uri())
            .build()
            .unwrap();
        client.list_models().await.unwrap();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let mut config = config();
        config.api_key = Some("fixture-github-token".into());
        let mut supplied = SuppliedTokenProvider::new(&config, &server.uri(), counter()).unwrap();
        supplied.driver = integration(&server);
        supplied.validate_credentials().await.unwrap();
        supplied.list_models().await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/exchange")
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn invalid_exchange_and_failed_live_lists_never_become_catalog_success() {
        for status in [401, 403, 500] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(json!({"error":"secret-token"})),
                )
                .mount(&server)
                .await;
            let error = integration(&server).exchange("invalid").await.unwrap_err();
            assert!(!error.to_string().contains("secret-token"));
            assert_eq!(
                error.to_string().contains("authentication rejected"),
                status != 500
            );
            let provider = current_provider(
                &server.uri(),
                &crate::credential::managed::integration::ResolvedSecret {
                    credentials: copilot::CopilotAuth::ApiKey("fixture-chat-token".into()),
                    expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                    cache: crate::credential::managed::integration::CachePolicy::NoCache,
                }
                .erase(),
                &counter(),
            )
            .unwrap();
            assert!(provider.validate_credentials().await.is_err());
            assert!(provider.list_models().await.is_err());
        }
    }

    #[tokio::test]
    async fn saved_protocol_and_trusted_endpoint_restrictions_are_explicit() {
        let driver = Driver::default();
        let connection = connection();
        assert!(driver.accepts(&connection));
        assert!(connection.supports_model_protocol("new-codex-model", ApiSurface::Responses));
        assert!(!connection.supports_model_protocol("new-codex-model", ApiSurface::Completions));
        assert!(!connection.supports_model_protocol("gpt-4o", ApiSurface::Responses));
        let mut custom = config();
        custom.base_url = Some("https://untrusted.example".into());
        let resolved =
            ProviderPlatform::resolve(&Handle::const_validated("account"), &custom).unwrap();
        assert!(!driver.accepts(&resolved));
        assert!(
            resolved
                .auth_methods
                .iter()
                .all(|method| method.method == CredentialMethod::ApiKey)
        );
        assert!(
            driver
                .build(
                    &resolved,
                    &custom,
                    &crate::credential::managed::integration::ResolvedSecret {
                        credentials: copilot::CopilotAuth::ApiKey("fixture-chat-token".into()),
                        expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                        cache: crate::credential::managed::integration::CachePolicy::NoCache,
                    }
                    .erase(),
                    &counter()
                )
                .is_err()
        );
        let invalid: ModelGroupConfig = serde_json::from_value(
            json!({"provider":"account","model":"fixture-codex","api":"completions"}),
        )
        .unwrap();
        assert!(
            crate::inference::config::compile_request("models.primary", &invalid, &config())
                .is_err()
        );
    }

    #[tokio::test]
    async fn explicit_id_selects_oauth_and_inline_keys_remain_independent() {
        use crate::inference::{
            credential::runtime::RuntimeCredentials, provider::validation::binding,
        };
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/exchange")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":"fixture-chat-token","expires_at":(Utc::now()+chrono::Duration::hours(1)).timestamp()}))).expect(1).mount(&server).await;
        let store =
            crate::credential::managed::test_support::credentials(db, "copilot-fixture-secret")
                .await
                .with_integrations(
                    crate::credential::managed::integration::register([(
                        "copilot".into(),
                        Arc::new(integration(&server))
                            as Arc<
                                dyn crate::credential::managed::integration::RegisteredIntegration,
                            >,
                    )])
                    .unwrap(),
                );
        let account = Handle::const_validated("account");
        let proof = store
            .stage(
                &account,
                CredentialMethod::Oauth,
                binding(&connection(), "database", &config()),
                &token(),
            )
            .await
            .unwrap();
        let saved = store.promote(proof.validation_id, Some(0)).await.unwrap();
        let mut managed_config = config();
        managed_config.credential_id = Some(saved.item_id);
        for _ in 0..2 {
            // Only the first resolution exchanges the stored GitHub token. Login never starts here.
            let runtime = RuntimeCredentials::new(
                [(account.clone(), managed_config.clone())].into(),
                store.clone(),
                counter(),
            );
            let selected = runtime.resolve_for_listing(&account).await.unwrap();
            assert_eq!(selected.method, CredentialMethod::Oauth);
            assert_eq!(selected.protocols, PROTOCOLS);
        }
        let mut supplied = config();
        supplied.api_key = Some("supplied-github-token".into());
        let runtime = RuntimeCredentials::new(
            [(account.clone(), supplied)].into(),
            store.clone(),
            counter(),
        );
        assert_eq!(
            runtime.resolve_for_listing(&account).await.unwrap().method,
            CredentialMethod::ApiKey
        );
        assert!(store.pending_status(&account).await.unwrap().is_empty());
    }
}
