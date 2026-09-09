//! Codex inference transport. Authentication lives in managed credentials.

use crate::core::config::{ApiSurface, ModelProviderConfig};
use crate::inference::credential::store::CredentialMethod;
use crate::inference::{
    credential::runtime::CredentialMethodDriver,
    error::InferenceError,
    protocol::http::WireClient,
    provider::{
        CredentialValidationError, InferenceCounter, ModelProvider, RigProvider,
        platform::ResolvedConnection,
    },
};
#[cfg(test)]
use chrono::{DateTime, Utc};
use rig_core::{
    model::{Model, ModelList},
    providers::chatgpt,
};
use std::sync::Arc;

const API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex";

#[derive(serde::Deserialize)]
struct ModelsResponse {
    models: Vec<DiscoveredModel>,
}

#[derive(serde::Deserialize)]
struct DiscoveredModel {
    slug: String,
    display_name: String,
    description: Option<String>,
    visibility: String,
    context_window: Option<u32>,
}

async fn list_models(
    client: &reqwest::Client,
    endpoint: &str,
    account_id: Option<&str>,
) -> Result<ModelList, CredentialValidationError> {
    let mut request = client
        .get(format!("{}/models", endpoint.trim_end_matches('/')))
        .query(&[("client_version", env!("CARGO_PKG_VERSION"))])
        .header("originator", "frona")
        .header("user-agent", concat!("frona/", env!("CARGO_PKG_VERSION")));
    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let response: ModelsResponse = super::discovery::get(request).await?;
    Ok(ModelList::new(
        response
            .models
            .into_iter()
            .filter(|model| model.visibility == "list")
            .map(|entry| {
                let mut model = Model::new(entry.slug, entry.display_name);
                model.description = entry.description;
                model.context_length = entry.context_window;
                model
            })
            .collect(),
    ))
}

#[derive(Default, Clone)]
pub struct Driver {
    #[cfg(test)]
    api_endpoint: Option<String>,
}

#[cfg(test)]
impl Driver {
    pub(crate) fn for_test(endpoint: String) -> Self {
        Self {
            api_endpoint: Some(endpoint),
        }
    }
}

fn current_provider(
    endpoint: &str,
    secret: &crate::credential::managed::integration::ErasedSecret,
    counter: &InferenceCounter,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    let credentials = secret
        .credentials::<chatgpt::ChatGPTAuth>()
        .map_err(|error| InferenceError::ConfigError(error.to_string()))?;
    let client =
        chatgpt::Client::builder()
            .api_key(credentials.clone())
            .allow_device_flow(false)
            .base_url(endpoint)
            .originator("frona")
            .user_agent(concat!("frona/", env!("CARGO_PKG_VERSION")))
            .default_instructions("")
            .http_client(WireClient::without_redirects().map_err(|_| {
                InferenceError::ConfigError("Cannot build ChatGPT transport".into())
            })?)
            .build()
            .map_err(|_| InferenceError::ConfigError("Cannot build ChatGPT client".into()))?;
    let chatgpt::ChatGPTAuth::AccessToken {
        access_token,
        account_id,
    } = credentials
    else {
        return Err(InferenceError::ConfigError(
            "ChatGPT discovery requires a managed access token".into(),
        ));
    };
    let discovery_client = super::discovery::client(access_token)?;
    let account_id = account_id.clone();
    let endpoint = endpoint.to_owned();
    Ok(Arc::new(
        RigProvider::new(client, counter.clone())
            .with_wire_transport()
            .with_live_check(move || {
                let client = discovery_client.clone();
                let endpoint = endpoint.clone();
                let account_id = account_id.clone();
                async move { list_models(&client, &endpoint, account_id.as_deref()).await }
            }),
    ))
}

#[async_trait::async_trait]
impl CredentialMethodDriver for Driver {
    fn method(&self) -> CredentialMethod {
        CredentialMethod::Oauth
    }

    fn accepts(&self, connection: &ResolvedConnection) -> bool {
        connection.brand == "openai"
            && connection.effective_base_url.as_deref() == Some("https://api.openai.com/v1")
    }

    fn protocols(&self) -> &[ApiSurface] {
        &[ApiSurface::Responses]
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
                "ChatGPT OAuth requires the trusted OpenAI connection".into(),
            ));
        }

        #[cfg(test)]
        let endpoint = self.api_endpoint.as_deref().unwrap_or(API_ENDPOINT);

        #[cfg(not(test))]
        let endpoint = API_ENDPOINT;
        current_provider(endpoint, secret, counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::managed::Candidate;
    use crate::credential::managed::{
        integration::openai_codex::{Document, OpenAiCodexIntegration},
        login::{ManagedLoginService, openai_codex::OpenAiCodexLogin, service::LoginTarget},
    };
    use crate::{
        chat::broadcast::BroadcastService,
        core::{
            Handle,
            config::{InferenceConfig, OpenAiApi, ProviderModel},
        },
        inference::{
            config::ModelRegistryConfig,
            credential::{
                runtime::RuntimeCredentials, setup::LoginStatus, store::ProviderCredentials,
            },
            provider::{ModelConfig, platform::ProviderPlatform},
        },
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use rig_core::completion::{
        AssistantContent, Message, ToolDefinition,
        message::{ToolResultContent, UserContent},
    };
    use serde_json::{Value, json};
    use std::time::Duration;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn counter() -> InferenceCounter {
        InferenceCounter::new(BroadcastService::new())
    }

    #[tokio::test]
    async fn discovery_uses_subscription_credentials_and_fresh_visible_model_ids() {
        use crate::inference::provider::{ProviderModelList, ProviderModelListSource};
        use wiremock::matchers::{header, query_param};
        let server = MockServer::start().await;
        let document = Document {
            access_token: "fixture-discovery-token".into(),
            refresh_token: None,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            account_id: Some("workspace-1".into()),
            scopes: vec![],
        };
        let provider = current_provider(
            &server.uri(),
            &document.resolved().unwrap().erase(),
            &counter(),
        )
        .unwrap();
        for body in [
            json!({"models":[
                {"slug":"new-subscription-model","display_name":"New model","visibility":"list","context_window":250000,"supported_in_api":false},
                {"slug":"hidden-model","display_name":"Hidden","visibility":"hide"}
            ]}),
            json!({"models":[]}),
        ] {
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .and(header("authorization", "Bearer fixture-discovery-token"))
                .and(header("chatgpt-account-id", "workspace-1"))
                .and(header("originator", "frona"))
                .and(query_param("client_version", env!("CARGO_PKG_VERSION")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
                .expect(1)
                .mount(&server)
                .await;
            let ProviderModelList::Listed { source, models } =
                provider.list_models().await.unwrap()
            else {
                panic!("expected live inventory")
            };
            assert_eq!(source, ProviderModelListSource::Account);
            if body["models"].as_array().unwrap().is_empty() {
                assert!(models.data.is_empty());
            } else {
                assert_eq!(models.data.len(), 1);
                assert_eq!(models.data[0].id, "new-subscription-model");
                assert_eq!(models.data[0].context_length, Some(250000));
            }
            server.verify().await;
        }
        for (status, body) in [
            (401, json!({"secret":"fixture-discovery-token"})),
            (200, json!({"wrong":[]})),
            (503, json!({})),
        ] {
            server.reset().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let error = provider.list_models().await.unwrap_err().to_string();
            assert!(!error.contains("fixture-discovery-token"));
            server.verify().await;
        }
    }

    fn config() -> ModelProviderConfig {
        ModelProviderConfig {
            provider: Some("openai".into()),
            ..Default::default()
        }
    }

    fn handle() -> Handle {
        Handle::const_validated("subscription")
    }

    fn connection() -> ResolvedConnection {
        ProviderPlatform::resolve(&handle(), &config()).unwrap()
    }

    fn jwt(expires: DateTime<Utc>, id: &str) -> String {
        format!("e30.{}.fixture",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp":expires.timestamp(),"https://api.openai.com/auth":{"chatgpt_account_id":id}})).unwrap()))
    }

    fn tokens(refresh: &str) -> Value {
        json!({"access_token":jwt(Utc::now()+chrono::Duration::hours(1),"account1"),"refresh_token":refresh})
    }

    fn payload() -> Candidate {
        Candidate::Secret(
            serde_json::to_value(
                crate::credential::managed::integration::openai_codex::Document {
                    access_token: jwt(Utc::now() + chrono::Duration::hours(1), "account1"),
                    refresh_token: Some("refresh1".into()),
                    expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                    account_id: Some("account1".into()),
                    scopes: vec![],
                },
            )
            .unwrap(),
        )
    }

    fn auth_driver(server: &MockServer) -> OpenAiCodexIntegration {
        OpenAiCodexIntegration {
            auth_endpoint: server.uri(),
            ..Default::default()
        }
    }

    fn driver(server: &MockServer) -> Driver {
        Driver {
            api_endpoint: Some(server.uri()),
            ..Default::default()
        }
    }

    async fn store() -> ProviderCredentials {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        crate::credential::managed::test_support::credentials(db, "fixture-encryption-key").await
    }

    fn model() -> ModelConfig {
        let group = serde_json::from_value(json!({"provider":"subscription","model":"manual-model","api":"responses","reasoning_effort":"low","extra_params":{"fixture":{"array":[1,null],"nullable":null},"text":{"verbosity":"low"}}})).unwrap();
        ModelRegistryConfig {
            providers: [(handle(), config())].into(),
            models: [("primary".into(), group)].into(),
            skip_auto_discover: true,
        }
        .parse_model_groups(&InferenceConfig::default(), Default::default())
        .unwrap()["primary"]
            .main
            .clone()
    }

    fn history() -> Vec<Message> {
        vec![
            Message::user("hello"),
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::tool_call_with_call_id(
                    "fc_old",
                    "call_old".into(),
                    "lookup",
                    json!({}),
                )],
            },
            Message::User {
                content: vec![UserContent::tool_result_with_call_id(
                    "fc_old",
                    "call_old",
                    "lookup",
                    vec![ToolResultContent::text("external-result")],
                )],
            },
        ]
    }

    fn sse(name: &str) -> String {
        let item = json!({"type":"function_call","id":"fc1","call_id":"call1","name":name,"arguments":"{\"ok\":true}","status":"completed"});
        [json!({"type":"response.output_item.added","output_index":0,"sequence_number":1,"item":{"type":"function_call","id":"fc1","call_id":"call1","name":name,"arguments":"","status":"in_progress"}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"sequence_number":2,"delta":"{\"ok\":true}"}),
            json!({"type":"response.output_item.done","output_index":0,"sequence_number":3,"item":item}),
            json!({"type":"response.completed","sequence_number":4,"response":{"id":"response1","object":"response","created_at":1,"status":"completed","model":"manual-model","output":[],"tools":[],"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}})]
            .iter().map(|event|format!("data: {event}\n\n")).collect::<String>()+"data: [DONE]\n\n"
    }

    #[tokio::test]
    async fn successful_backend_exchange_saves_credentials_without_an_extra_provider_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"device_auth_id":"device1","usercode":"ABCD","interval":"5"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"authorization_code":"code1","code_verifier":"verifier1"}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tokens("refresh1")))
            .expect(1)
            .mount(&server)
            .await;
        let store = store().await;
        let logins = ManagedLoginService::new([Arc::new(OpenAiCodexLogin {
            integration: auth_driver(&server),
        })
            as Arc<dyn crate::credential::managed::login::provider::LoginProvider>])
        .unwrap();
        let target = LoginTarget {
            vault: store.vault().clone(),
            key: crate::credential::managed::Key::new(handle().to_string(), "oauth"),
            metadata: serde_json::to_value(crate::inference::provider::validation::binding(
                &connection(),
                "database",
                &config(),
            ))
            .unwrap(),
            expected_id: None,
        };
        let attempt = logins.start("user", "openai_codex", target).await.unwrap();
        let public = serde_json::to_value(&attempt).unwrap();
        assert_eq!(
            public["challenge"]["url"],
            "https://auth.openai.com/codex/device"
        );
        assert!(
            logins
                .advance("user", attempt.id, None)
                .await
                .unwrap()
                .status
                == LoginStatus::Pending
        );
        tokio::time::sleep(Duration::from_millis(5050)).await;
        let completed = logins.advance("user", attempt.id, None).await.unwrap();
        assert!(completed.status == LoginStatus::Validated);
        assert!(
            !serde_json::to_string(&completed)
                .unwrap()
                .contains("refresh1")
        );
        assert_eq!(store.vault().list().await.unwrap().len(), 1);
        let credential_id = completed.credential_id.unwrap();
        for _ in 0..2 {
            let mut configured = config();
            configured.credential_id = Some(credential_id);
            let runtime =
                RuntimeCredentials::new([(handle(), configured)].into(), store.clone(), counter());
            assert_eq!(
                runtime.resolve_for_listing(&handle()).await.unwrap().method,
                CredentialMethod::Oauth
            );
        }
        assert!(
            logins
                .advance("user", attempt.id, Some("replay"))
                .await
                .is_err()
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let form = String::from_utf8(requests[2].body.clone()).unwrap();
        assert!(form.contains("code_verifier=verifier1"));
        assert!(form.contains("grant_type=authorization_code"));
    }

    #[tokio::test]
    async fn key_removal_keeps_saved_completions_unavailable_and_executes_the_saved_fallback() {
        use crate::db::repo::generic::SurrealRepo;
        use crate::inference::usage::{InferenceKind, UsageContext, UsageService};
        use frona_model_catalog::{ModelCatalogSnapshot, ModelCatalogStore};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"fallback","object":"chat.completion","created":1,"model":"fallback-model","choices":[{"index":0,"message":{"role":"assistant","content":"fallback-answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}))).expect(1).mount(&server).await;
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store =
            crate::credential::managed::test_support::credentials(db.clone(), "fixture").await;
        let binding =
            crate::inference::provider::validation::binding(&connection(), "database", &config());
        let mut credential_id = None;
        for (method, payload) in [
            (
                CredentialMethod::ApiKey,
                Candidate::Secret(
                    crate::credential::managed::integration::static_secret::document(
                        "original-key".into(),
                    ),
                ),
            ),
            (CredentialMethod::Oauth, payload()),
        ] {
            let proof = store
                .stage(&handle(), method, binding.clone(), &payload)
                .await
                .unwrap();
            let saved = store.promote(proof.validation_id, Some(0)).await.unwrap();
            if method == CredentialMethod::ApiKey {
                credential_id = Some(saved.item_id);
            }
        }
        let mut managed_config = config();
        managed_config.credential_id = credential_id;
        let configs=ModelRegistryConfig {providers:[(handle(),managed_config),(Handle::const_validated("backup"),ModelProviderConfig {provider:Some("openai".into()),base_url:Some(server.uri()),api_key:Some("backup-key".into()),..Default::default()})].into(),models:[("primary".into(),serde_json::from_value(json!({"provider":"subscription","model":"original-model","api":"completions","fallbacks":[{"provider":"backup","model":"fallback-model","api":"completions"}]})).unwrap())].into(),skip_auto_discover:true};
        let runtime = Arc::new(
            crate::inference::credential::runtime::RuntimeCredentials::new(
                configs.providers.clone(),
                store.clone(),
                crate::inference::provider::InferenceCounter::new(BroadcastService::new()),
            ),
        );
        let registry = {
            let registry_config = ModelRegistryConfig {
                providers: configs.providers.clone(),
                models: configs.models.clone(),
                skip_auto_discover: true,
            };
            let runtime = runtime.clone();
            let groups = registry_config
                .parse_model_groups_with_catalog(
                    &InferenceConfig::default(),
                    &frona_model_catalog::ModelCatalogSnapshot::empty(),
                    Arc::new(runtime.providers()),
                )
                .unwrap();
            crate::inference::provider::registry::ModelProviderRegistry::new(
                runtime.providers(),
                groups,
            )
        };
        let group = registry
            .resolve(&crate::inference::ModelRef::PRIMARY)
            .unwrap();
        assert!(
            registry
                .providers()
                .contains_key(group.main.provider_name())
        );
        let id = credential_id.unwrap();
        let current = store.vault().status_by_id(id).await.unwrap().unwrap();
        let Candidate::Secret(document) = payload() else {
            panic!("expected integration document")
        };
        store
            .vault()
            .replace_by_id(
                id,
                current.version,
                "openai_codex",
                current.metadata,
                document,
            )
            .await
            .unwrap();
        let unavailable = registry.unavailable_models().await;
        assert!(
            unavailable["primary"][0]
                .1
                .contains("unsupported_protocol_for_auth_method")
        );
        assert!(matches!(
            group.main.provider,
            ProviderModel::OpenAI {
                api: Some(OpenAiApi::ChatCompletions),
                ..
            }
        ));
        let usage = UsageService::new(
            ModelCatalogStore::new(ModelCatalogSnapshot::empty()),
            SurrealRepo::new(db),
            BroadcastService::new(),
        );
        let context = UsageContext::new(
            InferenceKind::Title {
                agent_id: "fixture".into(),
                chat_id: "fixture".into(),
            },
            "fixture-user",
            "fixture",
        );
        let result = group
            .inference(crate::inference::ModelRequest {
                system_prompt: "system",
                history: vec![Message::user("hello")],
                tools: vec![],
                usage_service: &usage,
                usage_context: &context,
                overrides: Default::default(),
            })
            .await
            .unwrap();
        assert!(result.content.iter().any(
            |item| matches!(item,AssistantContent::Text(text) if text.text=="fallback-answer")
        ));
        let restarted = {
            let registry_config = configs;
            let runtime = crate::inference::credential::runtime::RuntimeCredentials::new(
                registry_config.providers.clone(),
                store,
                crate::inference::provider::InferenceCounter::new(BroadcastService::new()),
            );
            let groups = registry_config
                .parse_model_groups_with_catalog(
                    &InferenceConfig::default(),
                    &frona_model_catalog::ModelCatalogSnapshot::empty(),
                    Arc::new(runtime.providers()),
                )
                .unwrap();
            crate::inference::provider::registry::ModelProviderRegistry::new(
                runtime.providers(),
                groups,
            )
        };
        assert!(
            restarted.unavailable_models().await["primary"][0]
                .1
                .contains("unsupported_protocol_for_auth_method")
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn refresh_rotates_tokens_rejects_account_switches_and_never_exposes_errors() {
        let server = MockServer::start().await;
        let driver = auth_driver(&server);
        let convert = |p: Candidate| {
            let Candidate::Secret(doc) = p else {
                panic!("expected document")
            };
            serde_json::from_value::<Document>(doc).unwrap()
        };
        let mut expired = convert(payload());
        expired.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(driver.refresh(&convert(payload())).await.unwrap().is_none());
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tokens("refresh2")))
            .mount(&server)
            .await;
        let refreshed = driver.refresh(&expired).await.unwrap().unwrap();
        assert!(
            matches!(refreshed,Document {refresh_token:Some(ref token),..} if token=="refresh2")
        );
        server.reset().await;
        for response in [ResponseTemplate::new(401).set_body_string("secret-refresh-token"), ResponseTemplate::new(200).set_body_json(json!({"access_token":"invalid","refresh_token":"r"})),ResponseTemplate::new(200).set_body_json(json!({"access_token":jwt(Utc::now()+chrono::Duration::hours(1),"other-account"),"refresh_token":"r"})),ResponseTemplate::new(200).set_body_json(json!({"access_token":jwt(Utc::now()-chrono::Duration::hours(1),"account1"),"refresh_token":"r"}))] {
            Mock::given(method("POST")).respond_with(response).mount(&server).await;
            let error=driver.refresh(&expired).await.err().unwrap().to_string();
            assert!(!error.contains("secret-refresh-token"));
            server.reset().await;
        }
        let mut response = tokens("unused");
        response.as_object_mut().unwrap().remove("refresh_token");
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
        assert!(
            matches!(driver.refresh(&expired).await.unwrap().unwrap(),Document {refresh_token:Some(ref token),..} if token=="refresh1")
        );
    }

    #[tokio::test]
    async fn custom_endpoints_cannot_use_codex_login_or_transport() {
        let server = MockServer::start().await;
        let driver = driver(&server);
        let mut config = config();
        config.base_url = Some(server.uri());
        let connection = ProviderPlatform::resolve(&handle(), &config).unwrap();
        assert!(!driver.accepts(&connection));
        assert!(
            crate::inference::credential::setup::provider(&connection, CredentialMethod::Oauth)
                .is_none()
        );
    }

    #[tokio::test]
    async fn actual_responses_transport_preserves_tools_streaming_structured_and_custom_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = request.body_json().unwrap();
                for field in ["max_output_tokens", "temperature"] {
                    if body.get(field).is_some() {
                        return ResponseTemplate::new(400).set_body_json(json!({
                            "detail": format!("Unsupported parameter: {field}")
                        }));
                    }
                }
                ResponseTemplate::new(200).set_body_raw(
                    sse(body["tools"][0]["name"].as_str().unwrap()),
                    "text/event-stream",
                )
            })
            .mount(&server)
            .await;
        let provider = current_provider(
            &server.uri(),
            &{
                let Candidate::Secret(doc) = payload() else {
                    panic!("expected document")
                };
                serde_json::from_value::<Document>(doc)
                    .unwrap()
                    .resolved()
                    .unwrap()
                    .erase()
            },
            &counter(),
        )
        .unwrap();
        let model = model();
        for streaming in [false, true] {
            let tools = vec![ToolDefinition {
                name: "lookup".into(),
                description: "Lookup".into(),
                parameters: json!({"type":"object","properties":{}}),
            }];
            let result = if streaming {
                let (tx, _rx) = tokio::sync::mpsc::channel(16);
                provider
                    .stream_inference(&model, "system", history(), tools, tx, None, None)
                    .await
            } else {
                provider
                    .inference(&model, "system", history(), tools, None, None)
                    .await
            }
            .unwrap();
            assert_eq!(result.usage.input_tokens, 2);
            assert_eq!(result.usage.output_tokens, 3);
            assert!(result.content.iter().any(|item|matches!(item,AssistantContent::ToolCall(call) if call.function.name=="lookup" && call.function.arguments==json!({"ok":true}))));
        }
        assert_eq!(
            provider
                .structured_inference(
                    &model,
                    "system",
                    history(),
                    json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}),
                    None,
                    None
                )
                .await
                .unwrap(),
            json!({"ok":true})
        );
        for request in server.received_requests().await.unwrap() {
            assert_eq!(request.headers["chatgpt-account-id"], "account1");
            assert!(
                request.headers["authorization"]
                    .to_str()
                    .unwrap()
                    .starts_with("Bearer e30.")
            );
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["model"], "manual-model");
            assert_eq!(body["stream"], true);
            assert_eq!(body["store"], false);
            assert_eq!(body["reasoning"]["effort"], "low");
            assert_eq!(body["fixture"], json!({"array":[1,null],"nullable":null}));
            assert!(body["input"].to_string().contains("external-result"));
            assert!(body["input"].to_string().contains("call_old"));
            assert!(body.get("max_output_tokens").is_none());
            assert!(body.get("temperature").is_none());
        }
        let mut explicit = model.clone();
        explicit.request_settings.max_tokens = Some(42);
        explicit.request_settings.temperature = Some(0.2);
        provider
            .structured_inference(
                &explicit,
                "system",
                history(),
                json!({"type":"object"}),
                None,
                None,
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = requests.last().unwrap().body_json().unwrap();
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("temperature").is_none());
        for mode in 0..3 {
            let tools = vec![ToolDefinition {
                name: "lookup".into(),
                description: "Lookup".into(),
                parameters: json!({"type":"object","properties":{}}),
            }];
            match mode {
                0 => {
                    provider
                        .inference(&explicit, "system", history(), tools, Some(1024), Some(0.0))
                        .await
                        .unwrap();
                }
                1 => {
                    let (tx, _rx) = tokio::sync::mpsc::channel(16);
                    provider
                        .stream_inference(
                            &explicit,
                            "system",
                            history(),
                            tools,
                            tx,
                            Some(1024),
                            Some(0.0),
                        )
                        .await
                        .unwrap();
                }
                _ => {
                    provider
                        .structured_inference(
                            &explicit,
                            "system",
                            history(),
                            json!({"type":"object"}),
                            Some(1024),
                            Some(0.0),
                        )
                        .await
                        .unwrap();
                }
            }
            let requests = server.received_requests().await.unwrap();
            let body: Value = requests.last().unwrap().body_json().unwrap();
            assert!(body.get("max_output_tokens").is_none());
            assert!(body.get("temperature").is_none());
        }
        let mut reserved = model;
        reserved
            .request_settings
            .extra_params
            .insert("stream".into(), json!(false));
        assert!(
            provider
                .inference(&reserved, "system", history(), vec![], None, None)
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 7);
    }
}
