//! Bedrock owns AWS identity resolution and signs the final Converse JSON.
//! Catalogs never participate in this transport.

use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::{
    Client,
    config::{Region, Token},
};
use aws_smithy_runtime_api::{
    box_error::BoxError,
    client::{
        auth::{
            AuthSchemeId, AuthSchemeOption, AuthSchemeOptionsFuture,
            http::HTTP_BEARER_AUTH_SCHEME_ID,
        },
        interceptors::{Intercept, context::BeforeTransmitInterceptorContextMut},
        runtime_components::RuntimeComponents,
    },
};
use aws_smithy_types::{body::SdkBody, config_bag::ConfigBag};
use rig_core::completion::{Message, request::ToolDefinition};
use tokio::sync::{OnceCell, mpsc};

use crate::core::config::ModelProviderConfig;
use crate::inference::{
    error::InferenceError,
    provider::{
        CredentialValidation, CredentialValidationError, InferenceCounter, InferenceOutput,
        ModelConfig, ModelListError, ModelProvider, ProviderModelList, ProviderModelListSource,
        RigProvider, StreamToken,
    },
};

/// Select exactly one authentication scheme. An unavailable explicit key must
/// never fall through to the deployment's IAM identity, or vice versa.
#[derive(Debug)]
struct SelectedAuth(bool);

impl aws_sdk_bedrock::config::auth::ResolveAuthScheme for SelectedAuth {
    fn resolve_auth_scheme<'a>(
        &'a self,
        _params: &'a aws_sdk_bedrock::config::auth::Params,
        _cfg: &'a ConfigBag,
        _runtime: &'a RuntimeComponents,
    ) -> AuthSchemeOptionsFuture<'a> {
        let id = if self.0 {
            HTTP_BEARER_AUTH_SCHEME_ID
        } else {
            AuthSchemeId::new("sigv4")
        };
        AuthSchemeOptionsFuture::ready(Ok(vec![
            AuthSchemeOption::builder()
                .scheme_id(id)
                .build()
                .expect("scheme ID is set"),
        ]))
    }
}

impl aws_sdk_bedrockruntime::config::auth::ResolveAuthScheme for SelectedAuth {
    fn resolve_auth_scheme<'a>(
        &'a self,
        _params: &'a aws_sdk_bedrockruntime::config::auth::Params,
        _cfg: &'a ConfigBag,
        _runtime: &'a RuntimeComponents,
    ) -> AuthSchemeOptionsFuture<'a> {
        let id = if self.0 {
            HTTP_BEARER_AUTH_SCHEME_ID
        } else {
            AuthSchemeId::new("sigv4")
        };
        AuthSchemeOptionsFuture::ready(Ok(vec![
            AuthSchemeOption::builder()
                .scheme_id(id)
                .build()
                .expect("scheme ID is set"),
        ]))
    }
}

#[derive(Debug)]
struct ConverseParameters;

/// Rig exposes only max_tokens/temperature through the typed AWS configuration.
/// Keep the unsupported standard settings here until its builder exposes them.
pub(crate) fn inference_settings(params: &crate::core::config::BedrockParams) -> serde_json::Value {
    let mut settings = serde_json::Map::new();
    if let Some(value) = params.top_p {
        settings.insert("topP".into(), serde_json::json!(value));
    }
    if let Some(value) = &params.stop_sequences {
        settings.insert("stopSequences".into(), serde_json::json!(value));
    }
    serde_json::json!({"inferenceConfig": settings})
}

impl Intercept for ConverseParameters {
    fn name(&self) -> &'static str {
        "FronaConverseParameters"
    }

    fn modify_before_signing(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let request = context.request_mut();
        let path = request.uri();
        if !path.ends_with("/converse") && !path.ends_with("/converse-stream") {
            return Ok(());
        }
        let bytes = request
            .body()
            .bytes()
            .ok_or("Converse request body is not buffered")?;
        let mut body: serde_json::Value = serde_json::from_slice(bytes)?;
        crate::inference::protocol::http::apply_current(&mut body);
        *request.body_mut() = SdkBody::from(serde_json::to_vec(&body)?);
        request.headers_mut().remove("content-length");
        Ok(())
    }
}

struct Connection {
    sdk: Client,
    discovery: aws_sdk_bedrock::Client,
    provider: RigProvider<rig_bedrock::client::Client>,
}

fn configure_loader(
    config: &ModelProviderConfig,
    mut loader: aws_config::ConfigLoader,
) -> aws_config::ConfigLoader {
    if let Some(profile) = &config.aws_profile {
        loader = loader.profile_name(profile);
    }
    if let Some(region) = &config.aws_region {
        loader = loader.region(Region::new(region.clone()));
    }
    if let Some(key) = config.api_key.as_ref().filter(|key| !key.is_empty()) {
        loader = loader
            .no_credentials()
            .token_provider(Token::new(key, None));
    }
    loader
}

pub(crate) struct BedrockProvider {
    config: ModelProviderConfig,
    counter: InferenceCounter,
    connection: OnceCell<Connection>,
}

impl BedrockProvider {
    pub fn new(config: ModelProviderConfig, counter: InferenceCounter) -> Self {
        Self {
            config,
            counter,
            connection: OnceCell::new(),
        }
    }

    async fn connection(&self) -> Result<&Connection, InferenceError> {
        self.connection.get_or_try_init(|| async {
            let key = self.config.api_key.as_ref().filter(|key| !key.is_empty());
            let loader = configure_loader(&self.config, aws_config::defaults(BehaviorVersion::latest()));
            let shared = loader.load().await;
            if shared.region().is_none() {
                return Err(InferenceError::ConfigError("Bedrock requires aws_region or a region from the selected AWS profile/environment".into()));
            }
            let mut builder = aws_sdk_bedrockruntime::config::Builder::from(&shared)
                .auth_scheme_resolver(SelectedAuth(key.is_some()))
                .retry_config(aws_sdk_bedrockruntime::config::retry::RetryConfig::disabled())
                .interceptor(ConverseParameters);
            if let Some(endpoint) = &self.config.base_url { builder = builder.endpoint_url(endpoint); }
            if let Some(key) = key { builder = builder.bearer_token(Token::new(key, None)); }
            let sdk = Client::from_conf(builder.build());
            let mut discovery = aws_sdk_bedrock::config::Builder::from(&shared)
                .auth_scheme_resolver(SelectedAuth(key.is_some()))
                .retry_config(aws_sdk_bedrock::config::retry::RetryConfig::disabled());
            if let Some(endpoint) = &self.config.base_url {
                discovery = discovery.endpoint_url(endpoint);
            }
            if let Some(key) = key { discovery = discovery.bearer_token(Token::new(key, None)); }
            let discovery = aws_sdk_bedrock::Client::from_conf(discovery.build());
            let provider = RigProvider::new(rig_bedrock::client::Client::from(sdk.clone()), self.counter.clone()).with_wire_transport();
            Ok(Connection { sdk, discovery, provider })
        }).await
    }

    #[cfg(test)]
    fn from_sdk(sdk: Client, counter: InferenceCounter) -> Self {
        // Inference fixtures never call discovery; listing fixtures replace this client.
        let discovery = aws_sdk_bedrock::Client::from_conf(
            aws_sdk_bedrock::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("eu-west-1"))
                .endpoint_url("http://127.0.0.1:9")
                .build(),
        );
        let provider = RigProvider::new(
            rig_bedrock::client::Client::from(sdk.clone()),
            counter.clone(),
        )
        .with_wire_transport();
        Self {
            config: ModelProviderConfig::default(),
            counter,
            connection: OnceCell::from(Connection {
                sdk,
                discovery,
                provider,
            }),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for BedrockProvider {
    async fn list_models(&self) -> Result<ProviderModelList, ModelListError> {
        use aws_sdk_bedrock::types::{InferenceType, ModelModality};
        use rig_core::model::{Model, ModelList};
        let connection = self
            .connection()
            .await
            .map_err(|_| ModelListError("AWS region/identity resolution failed".into()))?;
        let response = connection
            .discovery
            .list_foundation_models()
            .by_output_modality(ModelModality::Text)
            .by_inference_type(InferenceType::OnDemand)
            .send()
            .await
            .map_err(|error| discovery_error(error.raw_response().map(|r| r.status().as_u16())))?;
        let mut models: Vec<Model> = response
            .model_summaries()
            .iter()
            .map(|entry| {
                let mut model = Model::from_id(entry.model_id());
                model.name = entry.model_name().map(str::to_owned);
                model
            })
            .collect();
        // Cross-region and application inference profiles are callable model IDs too.
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let page = connection
                .discovery
                .list_inference_profiles()
                .set_next_token(cursor)
                .send()
                .await
                .map_err(|error| {
                    discovery_error(error.raw_response().map(|r| r.status().as_u16()))
                })?;
            models.extend(
                page.inference_profile_summaries()
                    .iter()
                    .filter(|entry| entry.status().as_str() == "ACTIVE")
                    .map(|entry| {
                        Model::new(entry.inference_profile_id(), entry.inference_profile_name())
                    }),
            );
            match page.next_token().filter(|token| !token.is_empty()) {
                Some(next) if seen.len() < 100 && seen.insert(next.to_owned()) => {
                    cursor = Some(next.to_owned())
                }
                Some(_) => return Err(ModelListError("Invalid Bedrock model pagination".into())),
                None => break,
            }
        }
        Ok(ProviderModelList::Listed {
            source: ProviderModelListSource::Account,
            models: ModelList::new(models),
        })
    }

    async fn validate_credentials(
        &self,
    ) -> Result<CredentialValidation, CredentialValidationError> {
        let connection = self.connection().await.map_err(|_| {
            CredentialValidationError::Failed("AWS region/identity resolution failed".into())
        })?;
        // Read-only, authenticated, regional runtime operation. This requires
        // bedrock:ListAsyncInvokes; it is not evidence of model entitlement.
        match connection
            .sdk
            .list_async_invokes()
            .max_results(1)
            .send()
            .await
        {
            Ok(_) => Ok(CredentialValidation { models: None }),
            Err(error) => match error
                .raw_response()
                .map(|response| response.status().as_u16())
            {
                Some(401 | 403) => Err(CredentialValidationError::AuthenticationRejected),
                Some(status) => Err(CredentialValidationError::Failed(format!(
                    "Bedrock returned HTTP {status}"
                ))),
                None => Err(CredentialValidationError::Failed(
                    "AWS credentials are unavailable or the regional request failed".into(),
                )),
            },
        }
    }

    async fn inference(
        &self,
        model: &ModelConfig,
        system: &str,
        history: Vec<Message>,
        tools: Vec<ToolDefinition>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        self.connection()
            .await?
            .provider
            .inference(model, system, history, tools, max_tokens, temperature)
            .await
    }

    async fn stream_inference(
        &self,
        model: &ModelConfig,
        system: &str,
        history: Vec<Message>,
        tools: Vec<ToolDefinition>,
        tx: mpsc::Sender<StreamToken>,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        self.connection()
            .await?
            .provider
            .stream_inference(model, system, history, tools, tx, max_tokens, temperature)
            .await
    }

    async fn structured_inference(
        &self,
        model: &ModelConfig,
        system: &str,
        history: Vec<Message>,
        schema: serde_json::Value,
        max_tokens: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        self.connection()
            .await?
            .provider
            .structured_inference(model, system, history, schema, max_tokens, temperature)
            .await
    }
}

fn discovery_error(status: Option<u16>) -> ModelListError {
    ModelListError(match status {
        Some(status) => format!("Bedrock model discovery returned HTTP {status}"),
        None => "Bedrock model discovery failed for the selected AWS identity and region".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::{
        config::ModelRegistryConfig,
        provider::{
            ProviderModelList,
            platform::{ProviderPlatform, build_provider},
        },
    };
    use crate::{
        chat::broadcast::BroadcastService,
        core::{
            Handle,
            config::{InferenceConfig, ModelGroupConfig},
        },
    };
    use aws_credential_types::{
        Credentials,
        provider::{ProvideCredentials, error::CredentialsError, future},
    };
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message as EventMessage};
    use serde_json::{Value, json};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn counter() -> InferenceCounter {
        InferenceCounter::new(BroadcastService::new())
    }

    fn sdk_builder(endpoint: &str, key: bool) -> aws_sdk_bedrockruntime::config::Builder {
        aws_sdk_bedrockruntime::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("eu-west-1"))
            .endpoint_url(endpoint)
            .credentials_provider(Credentials::new(
                "fixture-access",
                "fixture-secret",
                Some("fixture-session".into()),
                None,
                "fixture",
            ))
            .bearer_token(Token::new("fixture-bearer", None))
            .auth_scheme_resolver(SelectedAuth(key))
            .retry_config(aws_sdk_bedrockruntime::config::retry::RetryConfig::disabled())
            .interceptor(ConverseParameters)
    }

    fn listing_provider(endpoint: &str, key: bool) -> BedrockProvider {
        let mut provider = BedrockProvider::from_sdk(
            Client::from_conf(sdk_builder(endpoint, key).build()),
            counter(),
        );
        provider.connection.get_mut().unwrap().discovery = aws_sdk_bedrock::Client::from_conf(
            aws_sdk_bedrock::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("eu-west-1"))
                .endpoint_url(endpoint)
                .credentials_provider(Credentials::new(
                    "fixture-access",
                    "fixture-secret",
                    Some("fixture-session".into()),
                    None,
                    "fixture",
                ))
                .bearer_token(Token::new("fixture-bearer", None))
                .auth_scheme_resolver(SelectedAuth(key))
                .retry_config(aws_sdk_bedrock::config::retry::RetryConfig::disabled())
                .build(),
        );
        provider
    }

    #[tokio::test]
    async fn discovery_lists_regional_models_and_all_profile_pages_with_the_selected_identity() {
        use wiremock::matchers::query_param;
        for key in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("GET")).and(path("/foundation-models"))
                .and(query_param("byOutputModality", "TEXT")).and(query_param("byInferenceType", "ON_DEMAND"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"modelSummaries":[{"modelArn":"arn:aws:bedrock:eu-west-1::foundation-model/new-model","modelId":"new-model","modelName":"New model"}]})))
                .expect(1).mount(&server).await;
            Mock::given(method("GET")).and(path("/inference-profiles"))
                .respond_with(|request: &wiremock::Request| {
                    let second = request.url.query_pairs().any(|(key, value)| key == "nextToken" && value == "next&page=2");
                    if second { ResponseTemplate::new(200).set_body_json(json!({"inferenceProfileSummaries":[]})) }
                    else { ResponseTemplate::new(200).set_body_json(json!({"inferenceProfileSummaries":[{"inferenceProfileId":"eu.new-model","inferenceProfileArn":"arn:aws:bedrock:eu-west-1:123456789012:inference-profile/eu.new-model","inferenceProfileName":"Europe model","status":"ACTIVE","type":"SYSTEM_DEFINED","models":[]}],"nextToken":"next&page=2"})) }
                }).expect(2).mount(&server).await;
            let provider = listing_provider(&server.uri(), key);
            let ProviderModelList::Listed { models, .. } = provider.list_models().await.unwrap()
            else {
                panic!("expected live models")
            };
            assert_eq!(
                models
                    .data
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["new-model", "eu.new-model"]
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 3);
            for request in requests {
                let auth = request.headers["authorization"].to_str().unwrap();
                if key {
                    assert_eq!(auth, "Bearer fixture-bearer");
                } else {
                    assert!(auth.starts_with("AWS4-HMAC-SHA256"));
                    assert!(auth.contains("/eu-west-1/bedrock/aws4_request"));
                }
            }
        }
    }

    #[tokio::test]
    async fn discovery_empty_and_denied_results_never_fall_back_to_catalogs() {
        let server = MockServer::start().await;
        let provider = listing_provider(&server.uri(), true);
        for status in [200, 403, 500] {
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/foundation-models"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(if status == 200 {
                        json!({"modelSummaries":[]})
                    } else {
                        json!({"message":"fixture-bearer"})
                    }),
                )
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/inference-profiles"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"inferenceProfileSummaries":[]})),
                )
                .mount(&server)
                .await;
            let result = provider.list_models().await;
            if status == 200 {
                let ProviderModelList::Listed { models, .. } = result.unwrap() else {
                    panic!("expected live models")
                };
                assert!(models.data.is_empty());
            } else {
                assert!(!result.unwrap_err().to_string().contains("fixture-bearer"));
            }
        }
    }

    fn model(extra: Value) -> ModelConfig {
        let mut value = json!({"provider":"account","model":"fixture.model-v1:0","api":"amazon-bedrock-converse",
            "max_tokens":123,"temperature":0.2,"top_p":0.7,"stop_sequences":["END"]});
        value["extra_params"] = extra;
        let group: ModelGroupConfig = serde_json::from_value(value).unwrap();
        ModelRegistryConfig {
            providers: [(
                Handle::const_validated("account"),
                ModelProviderConfig {
                    provider: Some("amazon-bedrock".into()),
                    ..Default::default()
                },
            )]
            .into(),
            models: [("primary".into(), group)].into(),
            skip_auto_discover: true,
        }
        .parse_model_groups(&InferenceConfig::default(), Default::default())
        .unwrap()["primary"]
            .main
            .clone()
    }

    fn response(content: Value, stop: &str) -> Value {
        json!({"output":{"message":{"role":"assistant","content":content}},
            "stopReason":stop,"usage":{"inputTokens":3,"outputTokens":2,"totalTokens":5},"metrics":{"latencyMs":1}})
    }

    fn events() -> Vec<u8> {
        let mut bytes = Vec::new();
        for (kind, body) in [
            ("messageStart", json!({"role":"assistant"})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"text":"fixture reply"}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":0})),
            ("messageStop", json!({"stopReason":"end_turn"})),
            (
                "metadata",
                json!({"usage":{"inputTokens":3,"outputTokens":2,"totalTokens":5},"metrics":{"latencyMs":1}}),
            ),
        ] {
            let message = EventMessage::new(serde_json::to_vec(&body).unwrap())
                .add_header(Header::new(
                    ":message-type",
                    HeaderValue::String("event".into()),
                ))
                .add_header(Header::new(":event-type", HeaderValue::String(kind.into())))
                .add_header(Header::new(
                    ":content-type",
                    HeaderValue::String("application/json".into()),
                ));
            aws_smithy_eventstream::frame::write_message_to(&message, &mut bytes).unwrap();
        }
        bytes
    }

    #[tokio::test]
    async fn signed_and_bearer_converse_streaming_tools_and_native_parameters_reach_http() {
        for key in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/model/fixture.model-v1%3A0/converse"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(response(json!([{"text":"fixture reply"}]), "end_turn")),
                )
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/model/fixture.model-v1%3A0/converse-stream"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(events(), "application/vnd.amazon.eventstream"),
                )
                .mount(&server)
                .await;
            let provider = BedrockProvider::from_sdk(
                Client::from_conf(sdk_builder(&server.uri(), key).build()),
                counter(),
            );
            let model = model(
                json!({"inferenceConfig":{"topP":0.9},"additionalModelRequestFields":{"top_k":20,"nullable":null},"literal.dot":true}),
            );
            let tools = vec![ToolDefinition {
                name: "lookup".into(),
                description: "fixture".into(),
                parameters: json!({"type":"object","properties":{}}),
            }];
            let normal = provider
                .inference(
                    &model,
                    "system",
                    vec![Message::user("hello")],
                    tools.clone(),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(normal.usage.input_tokens, 3);
            let (tx, mut rx) = mpsc::channel(8);
            let stream = provider
                .stream_inference(
                    &model,
                    "system",
                    vec![Message::user("hello")],
                    tools,
                    tx,
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(stream.usage.output_tokens, 2);
            assert!(
                matches!(rx.recv().await, Some(StreamToken::Text(text)) if text == "fixture reply")
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests {
                let authorization = request
                    .headers
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap();
                if key {
                    assert_eq!(authorization, "Bearer fixture-bearer");
                } else {
                    assert!(
                        authorization.starts_with("AWS4-HMAC-SHA256 Credential=fixture-access/")
                    );
                    assert!(authorization.contains("/eu-west-1/bedrock/aws4_request"));
                    assert_eq!(request.headers["x-amz-security-token"], "fixture-session");
                }
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    body["inferenceConfig"],
                    // The AWS SDK's typed temperature is f32, serialized as f64.
                    json!({"maxTokens":123,"temperature":f64::from(0.2_f32),"topP":0.9,"stopSequences":["END"]})
                );
                assert_eq!(
                    body["additionalModelRequestFields"],
                    json!({"top_k":20,"nullable":null})
                );
                assert_eq!(body["literal.dot"], true);
                assert_eq!(body["system"][0]["text"], "system");
                assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "lookup");
                assert!(body.get("extra_params").is_none());
            }
        }
    }

    #[tokio::test]
    async fn structured_tool_results_and_reserved_paths_use_the_same_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response(
                json!([{"toolUse":{"toolUseId":"call-1","name":"submit","input":{"ok":true}}}]),
                "tool_use",
            )))
            .mount(&server)
            .await;
        let provider = BedrockProvider::from_sdk(
            Client::from_conf(sdk_builder(&server.uri(), true).build()),
            counter(),
        );
        let output = provider
            .structured_inference(
                &model(json!({"additionalModelRequestFields":{"top_k":3}})),
                "system",
                vec![Message::user("hello")],
                json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(output, json!({"ok":true}));
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "submit");
        assert!(body["toolConfig"]["toolChoice"].get("any").is_some());
        for key in [
            "modelId",
            "messages",
            "system",
            "toolConfig",
            "outputConfig",
        ] {
            let mut bad = model(json!({}));
            bad.request_settings
                .extra_params
                .insert(key.into(), Value::Null);
            assert!(
                provider
                    .structured_inference(
                        &bad,
                        "",
                        vec![Message::user("hello")],
                        json!({"type":"object"}),
                        None,
                        None
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn live_check_is_read_only_and_does_not_claim_model_inventory_or_fall_back_authentication()
     {
        for key in [true, false] {
            for status in [200, 403, 500] {
                let server = MockServer::start().await;
                Mock::given(method("GET"))
                    .and(path("/async-invoke"))
                    .respond_with(
                        ResponseTemplate::new(status).set_body_json(if status == 200 {
                            json!({"asyncInvokeSummaries":[]})
                        } else {
                            json!({"message":"fixture denied"})
                        }),
                    )
                    .mount(&server)
                    .await;
                let provider = BedrockProvider::from_sdk(
                    Client::from_conf(sdk_builder(&server.uri(), key).build()),
                    counter(),
                );
                let check = provider.validate_credentials().await;
                match status {
                    200 => assert!(check.unwrap().models.is_none()),
                    403 => assert!(matches!(
                        check,
                        Err(CredentialValidationError::AuthenticationRejected)
                    )),
                    _ => assert!(matches!(check, Err(CredentialValidationError::Failed(_)))),
                }
                let requests = server.received_requests().await.unwrap();
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].url.query(), Some("maxResults=1"));
                assert_eq!(
                    requests[0].headers["authorization"]
                        .to_str()
                        .unwrap()
                        .starts_with("Bearer "),
                    key
                );
            }
        }
    }

    #[tokio::test]
    async fn validation_proof_uses_the_exact_key_and_failed_candidates_never_stage() {
        use crate::inference::provider::validation::{
            CandidateCredential, ProviderValidationService, ValidationCandidate,
        };
        use surrealdb::{
            Surreal,
            engine::local::{Db, Mem},
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message":"denied"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer valid-fixture"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"asyncInvokeSummaries":[]})),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store =
            crate::credential::managed::test_support::credentials(db, "fixture-encryption").await;
        let service = ProviderValidationService::new(store.clone(), counter());
        let candidate = |key: &str| ValidationCandidate {
            handle: Handle::const_validated("bedrock-account"),
            config: ModelProviderConfig {
                provider: Some("amazon-bedrock".into()),
                aws_region: Some("eu-west-1".into()),
                base_url: Some(server.uri()),
                ..Default::default()
            },
            credential: CandidateCredential::New {
                method: crate::inference::credential::store::CredentialMethod::ApiKey,
                document: crate::credential::managed::integration::static_secret::document(
                    key.into(),
                ),
            },
        };
        assert!(service.validate(candidate("wrong-fixture")).await.is_err());
        assert!(store.vault().list().await.unwrap().is_empty());
        let proof = service.validate(candidate("valid-fixture")).await.unwrap();
        assert!(proof.models.is_none());
        assert!(store.vault().list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn selectors_round_trip_and_profile_region_is_used_unless_explicitly_overridden() {
        use aws_runtime::env_config::file::{
            EnvConfigFileKind as ProfileFileKind, EnvConfigFiles as ProfileFiles,
        };
        let config: ModelProviderConfig = serde_yaml::from_str(
            "provider: amazon-bedrock\naws_profile: production\napi_key: fixture-key\n",
        )
        .unwrap();
        let profiles = || {
            ProfileFiles::builder()
                .with_contents(
                    ProfileFileKind::Config,
                    "[default]\nregion = us-east-1\n[profile production]\nregion = eu-west-1\n",
                )
                .build()
        };
        let shared = configure_loader(
            &config,
            aws_config::defaults(BehaviorVersion::latest())
                .empty_test_environment()
                .profile_files(profiles()),
        )
        .load()
        .await;
        assert_eq!(shared.region().unwrap().as_ref(), "eu-west-1");
        assert!(shared.credentials_provider().is_none());
        let mut explicit = config.clone();
        explicit.aws_region = Some("ap-southeast-2".into());
        let shared = configure_loader(
            &explicit,
            aws_config::defaults(BehaviorVersion::latest())
                .empty_test_environment()
                .profile_files(profiles()),
        )
        .load()
        .await;
        assert_eq!(shared.region().unwrap().as_ref(), "ap-southeast-2");
        let persisted: ModelProviderConfig =
            serde_yaml::from_str(&serde_yaml::to_string(&explicit).unwrap()).unwrap();
        assert_eq!(persisted.aws_profile, explicit.aws_profile);
        assert_eq!(persisted.aws_region, explicit.aws_region);
        let resolved =
            ProviderPlatform::resolve(&Handle::const_validated("account"), &explicit).unwrap();
        assert!(build_provider(&resolved, &explicit, &counter()).is_ok());
    }

    #[derive(Debug, Clone)]
    struct Clock(Arc<AtomicU64>);

    impl aws_smithy_async::time::TimeSource for Clock {
        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(self.0.load(Ordering::SeqCst))
        }
    }

    #[derive(Debug)]
    struct TemporaryCredentials {
        clock: Clock,
        calls: Arc<AtomicUsize>,
        missing: bool,
    }

    impl ProvideCredentials for TemporaryCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::ready(if self.missing {
                Err(CredentialsError::not_loaded("fixture missing"))
            } else {
                let generation = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(Credentials::new(
                    format!("temporary-{generation}"),
                    "fixture-secret",
                    Some(format!("session-{generation}")),
                    Some(
                        aws_smithy_async::time::TimeSource::now(&self.clock)
                            + Duration::from_secs(3600),
                    ),
                    "fixture",
                ))
            })
        }
    }

    #[tokio::test]
    async fn sdk_refreshes_expired_temporary_credentials_and_missing_iam_does_not_use_a_bearer_token()
     {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"asyncInvokeSummaries":[]})),
            )
            .mount(&server)
            .await;
        let clock = Clock(Arc::new(AtomicU64::new(1_800_000_000)));
        let calls = Arc::new(AtomicUsize::new(0));
        let sdk = Client::from_conf(
            sdk_builder(&server.uri(), false)
                .time_source(clock.clone())
                .credentials_provider(TemporaryCredentials {
                    clock: clock.clone(),
                    calls: calls.clone(),
                    missing: false,
                })
                .build(),
        );
        let provider = BedrockProvider::from_sdk(sdk, counter());
        provider.validate_credentials().await.unwrap();
        provider.validate_credentials().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        clock.0.fetch_add(7200, Ordering::SeqCst);
        provider.validate_credentials().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests[0].headers["x-amz-security-token"], "session-0");
        assert_eq!(requests[2].headers["x-amz-security-token"], "session-1");
        let missing = BedrockProvider::from_sdk(
            Client::from_conf(
                sdk_builder(&server.uri(), false)
                    .credentials_provider(TemporaryCredentials {
                        clock,
                        calls,
                        missing: true,
                    })
                    .build(),
            ),
            counter(),
        );
        assert!(missing.validate_credentials().await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}
