//! Copilot inference transport. Managed credentials own token exchanges.

use crate::core::config::ModelProviderConfig;
use crate::credential::managed::integration::copilot::CopilotIntegration;
use crate::inference::credential::runtime::CredentialMethodDriver;
use chrono::{DateTime, Utc};
use frona_harness::{
    config::{ApiSurface, CredentialMethod},
    inference::{
        error::InferenceError,
        protocol::http::WireClient,
        provider::{
            CredentialValidationError, InferenceCounter, ModelConfig, ModelListError,
            ModelProvider, ProviderAdmin, ProviderConnection, RigProvider,
            platform::ResolvedConnection,
        },
    },
};
use rig_core::providers::copilot;
use std::sync::Arc;
use tokio::sync::Mutex;

use frona_harness::inference::provider::adapter::copilot::ENDPOINT;
#[cfg(test)]
use frona_harness::inference::provider::adapter::copilot::protocol;
const PROTOCOLS: &[ApiSurface] = &[ApiSurface::Completions, ApiSurface::Responses];

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
) -> Result<ProviderConnection, InferenceError> {
    let credentials = secret
        .credentials::<crate::credential::managed::integration::copilot::Credentials>()
        .map_err(|error| InferenceError::ConfigError(error.to_string()))?;
    let config = rig_core::providers::openai::OpenAIConfig::with_key(
        &copilot::DIALECT,
        credentials.0.clone(),
    )
    .with_base_url(endpoint);
    let check = config
        .clone()
        .connect(frona_harness::inference::provider::adapter::discovery::http_client()?);
    let client = config.connect(rig_core::http_client::DynHttpClient::new(
        WireClient::without_redirects()
            .map_err(|_| InferenceError::ConfigError("Cannot build Copilot HTTP client".into()))?,
    ));
    Ok(ProviderConnection {
        model: Arc::new(
            RigProvider::new(
                move |id: &str| client.completion(id).erase(),
                counter.clone(),
            )
            .with_wire_transport(),
        ),
        admin: ProviderAdmin::default().with_live_check(move || {
            let client = check.clone();
            async move {
                client
                    .list_models()
                    .await
                    .map_err(CredentialValidationError::from)
            }
        }),
    })
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
    ) -> Result<ProviderConnection, InferenceError> {
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
    provider: ProviderConnection,
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

    async fn provider(&self) -> Result<ProviderConnection, CredentialValidationError> {
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

    pub(crate) fn connection(self) -> ProviderConnection {
        let provider = Arc::new(self);
        let listing = provider.clone();
        let validation = provider.clone();
        ProviderConnection {
            model: provider,
            admin: ProviderAdmin::default()
                .with_listing_check(move || {
                    let provider = listing.clone();
                    async move {
                        provider
                            .provider()
                            .await
                            .map_err(|error| ModelListError(error.to_string()))?
                            .admin
                            .list_models()
                            .await
                    }
                })
                .with_validation(move || {
                    let provider = validation.clone();
                    async move {
                        provider
                            .provider()
                            .await?
                            .admin
                            .validate_credentials()
                            .await
                    }
                }),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for SuppliedTokenProvider {
    async fn resolve(
        &self,
        model: &ModelConfig,
    ) -> Result<frona_harness::inference::provider::ResolvedModel, InferenceError> {
        self.provider()
            .await
            .map_err(|error| InferenceError::InferenceFailed(error.to_string()))?
            .model
            .resolve(model)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::broadcast::BroadcastService, core::Handle, credential::managed::Candidate,
        inference::config::ModelRegistryConfig,
    };
    use frona_harness::{
        config::{InferenceConfig, ModelGroupConfig},
        inference::provider::ProviderModelList,
    };
    use rig_core::completion::AssistantContent;
    use rig_core::completion::{Message, ToolDefinition};
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
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
        }
    }

    fn config() -> ModelProviderConfig {
        ModelProviderConfig {
            provider: Some("github-copilot".into()),
            ..Default::default()
        }
    }

    fn connection() -> ResolvedConnection {
        crate::inference::provider::resolve_connection(
            &Handle::const_validated("account"),
            &config(),
        )
        .unwrap()
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
            Message::Assistant(rig_core::message::AssistantMessage::new(vec![
                AssistantContent::tool_call(
                    "previous-call",
                    "lookup".try_into().unwrap(),
                    json!({}),
                ),
            ])),
            Message::User {
                content: vec![rig_core::completion::message::UserContent::tool_result(
                    rig_core::message::CallId::from_wire("previous-call"),
                    "lookup".try_into().unwrap(),
                    vec![rig_core::completion::message::ToolResultContent::text(
                        "external-result",
                    )],
                )],
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
    async fn supplied_token_structured_results_keep_native_evidence_on_both_routes() {
        use frona_harness::inference::{observation::Observations, provider::RigModelAdapter};
        use frona_harness::{DefaultBudget, ExecutionControl, ModelAdapter, ModelRequest};
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/exchange"))
            .and(header("authorization", "token fixture-github-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":"fixture-chat-token","expires_at":(Utc::now()+chrono::Duration::hours(1)).timestamp()})))
            .expect(1).mount(&server).await;
        let mut config = config();
        config.api_key = Some("fixture-github-token".into());
        let mut provider = SuppliedTokenProvider::new(&config, &server.uri(), counter()).unwrap();
        provider.driver = integration(&server);
        let provider = Arc::new(provider);
        let schema =
            json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]});
        for id in ["fixture-chat", "fixture-codex"] {
            let responses = id == "fixture-codex";
            let model = model(id);
            let adapter = RigModelAdapter::new(provider.clone(), model.clone());
            for (name, arguments, incomplete, valid) in [
                ("submit", "{\"ok\":true}", false, true),
                ("submit", "{broken", false, false),
                ("submit", "{\"ok\":1}", false, false),
                ("not_submit", "{\"ok\":true}", false, false),
                ("submit", "{\"ok\":true}", true, false),
            ] {
                let mut body = response(responses, name);
                if responses {
                    body["output"][0]["arguments"] = json!(arguments);
                    if incomplete {
                        body["status"] = json!("incomplete");
                        body["incomplete_details"] = json!({"reason":"max_output_tokens"});
                    }
                } else {
                    body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
                        json!(arguments);
                    if incomplete {
                        body["choices"][0]["finish_reason"] = json!("length");
                    }
                }
                let scoped = Mock::given(method("POST"))
                    .and(path(if responses {
                        "/responses"
                    } else {
                        "/chat/completions"
                    }))
                    .and(header("authorization", "Bearer fixture-chat-token"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body))
                    .expect(1)
                    .mount_as_scoped(&server)
                    .await;
                let budget = DefaultBudget::default();
                let output = ExecutionControl::new(&budget)
                    .model_attempt(
                        &model,
                        Observations::default(),
                        || {
                            adapter.generate(
                                ModelRequest {
                                    system: "system".into(),
                                    messages: vec![Message::user("answer")],
                                    tools: vec![],
                                    schema: Some(schema.clone()),
                                    max_tokens: None,
                                    temperature: None,
                                },
                                None,
                            )
                        },
                        |output| output.usage.clone(),
                    )
                    .await
                    .unwrap();
                assert_eq!(budget.snapshot().tokens, 5);
                assert_eq!(output.usage.observed().unwrap().total_tokens, Some(5));
                assert_eq!(
                    output.metadata["response_id"],
                    if responses { "response1" } else { "chat1" }
                );
                assert!(output.origin.is_some());
                assert!(
                    output
                        .content
                        .iter()
                        .any(|part| matches!(part, AssistantContent::ToolCall(_)))
                );
                match output.validate(Some(&schema)) {
                    Ok(()) => {
                        assert!(valid);
                        assert_eq!(output.structured, Some(json!({"ok":true})));
                    }
                    Err(InferenceError::InvalidOutput { output: failed, .. }) => {
                        assert!(!valid);
                        assert_eq!(
                            serde_json::to_value(&*failed).unwrap(),
                            serde_json::to_value(&output).unwrap()
                        );
                    }
                    result => panic!("unexpected {id} validation: {result:?}"),
                }
                drop(scoped);
            }
        }
        server.verify().await;
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
                credentials: crate::credential::managed::integration::copilot::Credentials(
                    "fixture-chat-token".into(),
                ),
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
                    name: rig_core::message::ToolName::new("lookup").unwrap(),
                    description: "Lookup".into(),
                    parameters: json!({"type":"object","properties":{}}),
                    annotations: Default::default(),
                }];
                let output = if streaming {
                    let (tx, _rx) = mpsc::channel(16);
                    crate::provider_fixture::invoke(
                        provider.model.clone(),
                        &model,
                        frona_harness::ModelRequest {
                            system: ("system").into(),
                            messages: tool_history(),
                            tools,
                            schema: None,
                            max_tokens: None,
                            temperature: None,
                        },
                        Some(tx),
                    )
                    .await
                } else {
                    crate::provider_fixture::invoke(
                        provider.model.clone(),
                        &model,
                        frona_harness::ModelRequest {
                            system: ("system").into(),
                            messages: tool_history(),
                            tools,
                            schema: None,
                            max_tokens: None,
                            temperature: None,
                        },
                        None,
                    )
                    .await
                }
                .unwrap();
                assert_eq!(output.usage.observed().unwrap().input_tokens, Some(2));
                assert_eq!(output.usage.observed().unwrap().output_tokens, Some(3));
                assert!(output.content.iter().any(|content|matches!(content,AssistantContent::ToolCall(call) if call.function.name=="lookup" && serde_json::Value::Object(call.function.arguments.clone())==json!({"ok":true}))));
            }
            assert_eq!(crate::provider_fixture::invoke(provider.model.clone(), &model, frona_harness::ModelRequest { system: ("system").into(), messages: vec![Message::user("hello")], tools: vec![], schema: Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]})), max_tokens: None, temperature: None }, None).await.map(|output| output.structured.expect("validated structured output")).unwrap(),json!({"ok":true}));
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
            provider.admin.validate_credentials().await.unwrap().models,
            Some(ProviderModelList::Listed { .. })
        ));
        assert!(matches!(
            provider.admin.list_models().await.unwrap(),
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
        // Resolved tokens use a wire directly, with no authenticator or token store.
        provider.admin.list_models().await.unwrap();
        let mut config = config();
        config.api_key = Some("fixture-github-token".into());
        let mut supplied = SuppliedTokenProvider::new(&config, &server.uri(), counter()).unwrap();
        supplied.driver = integration(&server);
        let supplied = supplied.connection();
        supplied.admin.validate_credentials().await.unwrap();
        supplied.admin.list_models().await.unwrap();
        // Administration and execution share the same derived token cache.
        supplied
            .model
            .resolve(&model("fixture-chat"))
            .await
            .unwrap();
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
                    credentials: crate::credential::managed::integration::copilot::Credentials(
                        "fixture-chat-token".into(),
                    ),
                    expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                    cache: crate::credential::managed::integration::CachePolicy::NoCache,
                }
                .erase(),
                &counter(),
            )
            .unwrap();
            assert!(provider.admin.validate_credentials().await.is_err());
            assert!(provider.admin.list_models().await.is_err());
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
        let resolved = crate::inference::provider::resolve_connection(
            &Handle::const_validated("account"),
            &custom,
        )
        .unwrap();
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
                        credentials: crate::credential::managed::integration::copilot::Credentials(
                            "fixture-chat-token".into()
                        ),
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
            crate::inference::config::ModelRegistryConfig {
                providers: [(crate::core::Handle::const_validated("account"), config())].into(),
                models: [("primary".into(), invalid)].into(),
                skip_auto_discover: true,
            }
            .validate_model_groups()
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
