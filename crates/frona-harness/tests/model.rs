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
                UsageReport::Reported(Usage::default())
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
            total_tokens: 0,
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
            name: "read".into(),
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
        UsageReport::LegacyNormalized(Usage {
            total_tokens: 0,
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
    assert_eq!(call.id, "call-original");
    assert_eq!(call.function.arguments, arguments);
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
    assert_eq!(call.signature.as_deref(), Some(signature));
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
            name: "read".into(),
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
