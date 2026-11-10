use async_trait::async_trait;
use frona_harness::{
    Capabilities, Capability, Model, ModelAdapter, ModelError, ModelOutput, ModelRequest,
    UsageReport,
    config::ApiSurface,
    inference::{Usage, error::InferenceError, provider::StreamToken},
};
use rig_core::completion::AssistantContent;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct ApplicationClient<'a> {
    calls: &'a AtomicUsize,
    usage: bool,
}

#[async_trait]
impl ModelAdapter for ApplicationClient<'_> {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured: true,
            streaming: true,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(tokens) = tokens {
            let _ = tokens.send(StreamToken::Text("hello".into())).await;
        }
        let mut response = ModelOutput::new(
            vec![AssistantContent::text("hello")],
            if self.usage {
                UsageReport::Reported(Usage::new().total_tokens(0))
            } else {
                UsageReport::Unavailable
            },
        );
        response.metadata = json!({"request_id":"application-original"});
        if request.schema.is_some() {
            response.structured = Some(json!({"value":7}));
        }
        Ok(response)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Answer {
    value: usize,
}

#[tokio::test]
async fn application_client_borrows_state_and_preserves_usage_and_metadata() {
    let calls = AtomicUsize::new(0);
    let model = Model::from_adapter(
        "gateway",
        "app-model",
        ApplicationClient {
            calls: &calls,
            usage: true,
        },
    )
    .unwrap();
    let output = model.complete(ModelRequest::new("hi")).await.unwrap();
    assert!(matches!(
        output.usage,
        UsageReport::Reported(Usage {
            total_tokens: Some(0),
            ..
        })
    ));
    assert_eq!(output.metadata["request_id"], "application-original");
    let typed = model
        .complete_typed::<Answer>(ModelRequest::new("plan"))
        .await
        .unwrap();
    assert_eq!(typed.value.value, 7);
    assert_eq!(typed.response.structured, Some(json!({"value":7})));
    let (tx, mut rx) = mpsc::channel(1);
    let (output, token) = tokio::join!(model.stream(ModelRequest::new("stream"), tx), rx.recv());
    assert!(matches!(token, Some(StreamToken::Text(text)) if text == "hello"));
    assert_eq!(
        output.unwrap().metadata["request_id"],
        "application-original"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    drop(model);
    let missing = Model::from_adapter(
        "gateway",
        "app-model",
        ApplicationClient {
            calls: &calls,
            usage: false,
        },
    )
    .unwrap();
    assert!(matches!(
        missing
            .complete(ModelRequest::new("hi"))
            .await
            .unwrap()
            .usage,
        UsageReport::Unavailable
    ));
}

#[tokio::test]
async fn unsupported_capabilities_fail_before_calling_the_application() {
    let calls = AtomicUsize::new(0);
    let model = Model::from_adapter(
        "gateway",
        "text-model",
        ApplicationClient {
            calls: &calls,
            usage: true,
        },
    )
    .unwrap();
    let mut request = ModelRequest::new("hi");
    request
        .tools
        .push(rig_core::completion::request::ToolDefinition {
            annotations: Default::default(),
            name: "read".try_into().unwrap(),
            description: "read".into(),
            parameters: json!({"type":"object"}),
        });
    assert!(
        matches!(model.complete(request).await, Err(ModelError::Unsupported { capability: Capability::Tools, model }) if model == "gateway/text-model")
    );
    let mut request = ModelRequest::new("hi");
    request.schema = Some(json!({"type":"object"}));
    let (tx, _rx) = mpsc::channel(1);
    assert!(matches!(
        model.stream(request, tx).await,
        Err(ModelError::Unsupported {
            capability: Capability::StructuredStreaming,
            ..
        })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

fn chat_response(content: serde_json::Value) -> serde_json::Value {
    json!({"id":"chat-original","object":"chat.completion","created":0,"model":"mock-model","choices":[{"index":0,"message":content,"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}})
}

#[tokio::test]
async fn builtin_openai_uses_existing_factory_and_preserves_raw_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_response(json!({"role":"assistant","content":"hello"}))),
        )
        .expect(1)
        .mount(&server)
        .await;
    let model = Model::builder("openai", "mock-model")
        .api_key("fixture-key")
        .base_url(server.uri())
        .api(ApiSurface::Completions)
        .build()
        .unwrap();
    let output = model.complete(ModelRequest::new("hi")).await.unwrap();
    assert!(matches!(&output.content[0], AssistantContent::Text(text) if text.text == "hello"));
    assert_eq!(output.metadata["response_id"], "chat-original");
    assert_eq!(output.metadata["raw"]["id"], "chat-original");
    assert!(matches!(
        output.usage,
        UsageReport::Reported(Usage {
            total_tokens: Some(0),
            ..
        })
    ));
}

#[tokio::test]
async fn builtin_structured_output_keeps_call_ids_and_original_arguments() {
    let server = MockServer::start().await;
    let arguments = json!({"value":9,"description":"legitimate field"});
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response(json!({"role":"assistant","content":null,"tool_calls":[{"id":"call-original","type":"function","function":{"name":"submit","arguments":arguments.to_string()}}]}))))
        .expect(1).mount(&server).await;
    let model = Model::builder("openai", "mock-model")
        .api_key("fixture-key")
        .base_url(server.uri())
        .api(ApiSurface::Completions)
        .build()
        .unwrap();
    let output = model
        .complete_typed::<Answer>(ModelRequest::new("plan"))
        .await
        .unwrap();
    assert_eq!(output.value.value, 9);
    assert_eq!(output.response.structured, Some(arguments.clone()));
    let call = output
        .response
        .content
        .iter()
        .find_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .unwrap();
    assert_eq!(call.id.wire(), "call-original");
    assert_eq!(call.function.arguments_value(), arguments);
    assert_eq!(
        output.response.metadata["raw"]["choices"][0]["message"]["tool_calls"][0]["id"],
        "call-original"
    );
}

#[tokio::test]
async fn structured_decode_failure_retains_original_response() {
    let calls = AtomicUsize::new(0);
    let model = Model::from_adapter(
        "gateway",
        "app-model",
        ApplicationClient {
            calls: &calls,
            usage: false,
        },
    )
    .unwrap();
    #[derive(Deserialize, JsonSchema)]
    struct Wrong {
        _required: bool,
    }
    let Err(ModelError::Structured { response, .. }) =
        model.complete_typed::<Wrong>(ModelRequest::new("hi")).await
    else {
        panic!("expected typed decode failure")
    };
    assert_eq!(response.structured, Some(json!({"value":7})));
    assert_eq!(response.metadata["request_id"], "application-original");
}

#[test]
fn builtin_construction_does_not_initialize_managed_credentials() {
    assert!(
        Model::builder("github-copilot", "model")
            .api_key("fixture")
            .build()
            .is_err()
    );
    assert!(Model::builder("openai", "model").build().is_err());
    assert!(
        Model::builder("openai", "")
            .api_key("fixture")
            .build()
            .is_err()
    );
}

#[tokio::test]
async fn builtin_signed_submission_preserves_provider_signature() {
    let server = MockServer::start().await;
    let signature = "opaque-A+/=\nunchanged";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"submit","args":{"value":11}},"thoughtSignature":signature}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"totalTokenCount":13}
        }))).expect(1).mount(&server).await;
    let model = Model::builder("google", "mock-model")
        .api_key("fixture-key")
        .base_url(server.uri())
        .build()
        .unwrap();
    let output = model
        .complete_typed::<Answer>(ModelRequest::new("plan"))
        .await
        .unwrap();
    assert_eq!(output.value.value, 11);
    let call = output
        .response
        .content
        .iter()
        .find_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        call.native.as_ref().unwrap().item["thoughtSignature"],
        signature
    );
    assert_eq!(
        output.response.metadata["raw"]["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
        signature
    );
}

#[tokio::test]
async fn builtin_does_not_silently_drop_tools_from_native_structured_requests() {
    let server = MockServer::start().await;
    let model = Model::builder("openai", "mock-model")
        .api_key("fixture-key")
        .base_url(server.uri())
        .build()
        .unwrap();
    let mut request = ModelRequest::new("plan");
    request.schema = Some(json!({"type":"object"}));
    request
        .tools
        .push(rig_core::completion::request::ToolDefinition {
            annotations: Default::default(),
            name: "read".try_into().unwrap(),
            description: "read".into(),
            parameters: json!({"type":"object"}),
        });
    assert!(matches!(
        model.complete(request).await,
        Err(ModelError::Unsupported {
            capability: Capability::StructuredTools,
            ..
        })
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn owned_application_task_is_spawnable() {
    let output = tokio::spawn(async {
        let calls = AtomicUsize::new(0);
        let model = Model::from_adapter(
            "gateway",
            "app-model",
            ApplicationClient {
                calls: &calls,
                usage: false,
            },
        )
        .unwrap();
        model.complete(ModelRequest::new("hi")).await
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output.metadata["request_id"], "application-original");
}

#[tokio::test]
async fn supplied_rig_model_uses_its_transport_and_preserves_native_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response(
            json!({"role":"assistant","content":"from Rig"}),
        )))
        .expect(1)
        .mount(&server)
        .await;
    let native = rig_core::providers::openai::OpenAIConfig::new("fixture")
        .with_base_url(server.uri())
        .client()
        .chat("unlisted-private-model");
    let model = Model::from_rig(native).unwrap();
    let mut request = ModelRequest::new("hi");
    request.max_tokens = Some(123);
    request.temperature = Some(0.4);
    let output = model.complete(request).await.unwrap();
    assert_eq!(model.config().model_id, "unlisted-private-model");
    assert_eq!(output.metadata["response_id"], "chat-original");
    assert!(output.origin.is_some());
    assert!(matches!(
        output.usage,
        UsageReport::Reported(Usage {
            total_tokens: Some(0),
            ..
        })
    ));
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = requests[0].body_json().unwrap();
    assert_eq!(body["model"], "unlisted-private-model");
    // The supplied Rig transport keeps its own mapping for unknown model IDs.
    assert_eq!(body["max_tokens"], 123);
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!(body["temperature"], 0.4);
}

#[tokio::test]
async fn invalid_native_submissions_keep_response_and_charge_usage_before_rejection() {
    use frona_harness::inference::observation::Observations;
    use frona_harness::{BudgetLimits, DefaultBudget, ExecutionControl, ReportHandle};
    for (message, stop) in [
        (
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"bad-json","type":"function","function":{"name":"submit","arguments":"{broken"}}]}),
            "tool_calls",
        ),
        (
            json!({"role":"assistant","content":"no submission"}),
            "stop",
        ),
        (
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"bad-schema","type":"function","function":{"name":"submit","arguments":"{\"value\":\"wrong type\"}"}}]}),
            "tool_calls",
        ),
        (
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"unfinished","type":"function","function":{"name":"submit","arguments":"{\"value\":9}"}}]}),
            "length",
        ),
    ] {
        let server = MockServer::start().await;
        let mut native = chat_response(message.clone());
        native["choices"][0]["finish_reason"] = json!(stop);
        native["usage"] = json!({"prompt_tokens":5,"completion_tokens":2,"total_tokens":7});
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(native.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let model = Model::builder("openai", "mock-model")
            .api_key("fixture-key")
            .base_url(server.uri())
            .api(ApiSurface::Completions)
            .build()
            .unwrap();
        let budget = DefaultBudget::new(BudgetLimits {
            model_attempts: Some(1),
            ..Default::default()
        });
        let report = ReportHandle::default();
        let mut request = ModelRequest::new("plan");
        request.schema = Some(schemars::schema_for!(Answer).to_value());
        let error = model
            .invoke_controlled(
                request,
                None,
                Observations::default(),
                ExecutionControl::new(&budget).with_report(&report),
            )
            .await
            .unwrap_err();
        let ModelError::Structured { response, .. } = error else {
            panic!("expected failure retaining structured response: {error:?}");
        };
        assert_eq!(budget.snapshot().tokens, 7);
        assert_eq!(budget.snapshot().model_attempts, 1);
        assert_eq!(budget.snapshot().in_flight, 0);
        assert_eq!(report.snapshot().attempts.len(), 1);
        assert_eq!(response.metadata["raw"], native);
        assert_eq!(response.metadata["response_id"], "chat-original");
        assert!(response.origin.is_some());
        assert!(!response.content.is_empty());
        assert!(matches!(
            response.usage,
            UsageReport::Reported(Usage {
                total_tokens: Some(7),
                ..
            })
        ));
        if stop == "length" {
            assert!(matches!(
                response.stop,
                Some(rig_core::message::StopReason::Length)
            ));
        }
    }
}

#[tokio::test]
async fn supplied_rig_stream_retains_final_usage_and_measures_first_token() {
    let server = MockServer::start().await;
    let chunk = |delta: serde_json::Value, finish: serde_json::Value, usage: serde_json::Value| json!({"id":"stream-original","object":"chat.completion.chunk","created":0,"model":"mock-model","choices":[{"index":0,"delta":delta,"finish_reason":finish}],"usage":usage});
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            json!({"role":"assistant","content":"hello"}),
            json!(null),
            json!(null)
        ),
        chunk(
            json!({}),
            json!("stop"),
            json!({"prompt_tokens":5,"completion_tokens":2,"total_tokens":7})
        ),
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let native = rig_core::providers::openai::OpenAIConfig::new("fixture")
        .with_base_url(server.uri())
        .client()
        .chat("mock-model");
    let model = Model::from_rig(native).unwrap();
    let (tx, mut rx) = mpsc::channel(8);
    let output = model.stream(ModelRequest::new("hi"), tx).await.unwrap();
    assert!(matches!(rx.recv().await, Some(StreamToken::Text(text)) if text == "hello"));
    assert!(output.ttft_ms.is_some());
    assert_eq!(output.metadata["response_id"], "stream-original");
    assert!(matches!(
        output.usage,
        UsageReport::Reported(Usage {
            total_tokens: Some(7),
            ..
        })
    ));
    assert!(output.origin.is_some());
}

#[tokio::test]
async fn denied_standalone_attempt_does_not_preflight_or_resolve_credentials() {
    use frona_harness::inference::{
        observation::Observations,
        provider::{ModelConfig, ModelProvider, ResolvedModel, RigModelAdapter},
    };
    use frona_harness::{BudgetLimits, DefaultBudget, ExecutionControl, Handle};
    use std::sync::Arc;

    struct Credentials {
        preflights: Arc<AtomicUsize>,
        resolutions: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ModelProvider for Credentials {
        async fn ensure_usable(&self, _: &ModelConfig) -> Result<(), InferenceError> {
            self.preflights.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn resolve(&self, _: &ModelConfig) -> Result<ResolvedModel, InferenceError> {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            Err(InferenceError::InferenceFailed(
                "credential resolution reached".into(),
            ))
        }
    }
    let preflights = Arc::new(AtomicUsize::new(0));
    let resolutions = Arc::new(AtomicUsize::new(0));
    let config = ModelConfig {
        catalog_provider: "fixture".into(),
        provider_handle: Handle::const_validated("fixture"),
        model_id: "model".into(),
        provider: frona_harness::config::ProviderModel::Generic,
        request_settings: Default::default(),
    };
    let adapter = RigModelAdapter::new(
        Arc::new(Credentials {
            preflights: preflights.clone(),
            resolutions: resolutions.clone(),
        }),
        config,
    );
    let model = Model::from_adapter("fixture", "model", adapter).unwrap();
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    let error = model
        .invoke_controlled(
            ModelRequest::new("hello"),
            None,
            Observations::default(),
            ExecutionControl::new(&budget),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ModelError::Provider(InferenceError::Stop(
            frona_harness::ExecutionStop::BudgetExhausted(_)
        ))
    ));
    assert_eq!(preflights.load(Ordering::SeqCst), 0);
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);
}
