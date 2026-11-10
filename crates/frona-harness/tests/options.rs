//! Wire compatibility for settings compiled into Rig's typed request options.
use std::sync::Arc;

use frona_harness::{
    Handle, ModelAdapter, ModelRequest,
    config::{ModelGroupConfig, ModelProviderConfig},
    inference::{
        config::ModelRegistryConfig,
        provider::{
            InferenceCounter, ModelConfig, ModelProvider, RigModelAdapter,
            platform::{ProviderPlatform, build_provider},
        },
    },
};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn configured(
    brand: &str,
    endpoint: &str,
    settings: Value,
) -> (Arc<dyn ModelProvider>, ModelConfig) {
    let handle = Handle::const_validated("account");
    let connection = ModelProviderConfig {
        provider: Some(brand.into()),
        api_key: Some("fixture-key".into()),
        base_url: Some(endpoint.into()),
        ..Default::default()
    };
    let mut settings = settings.as_object().unwrap().clone();
    settings.insert("provider".into(), json!(handle.as_str()));
    settings.entry("max_tokens").or_insert(json!(123));
    let recipe: ModelGroupConfig = serde_json::from_value(Value::Object(settings)).unwrap();
    let resolved = ProviderPlatform::resolve(&handle, &connection).unwrap();
    let provider = build_provider(&resolved, &connection, &InferenceCounter::default()).unwrap();
    let groups = ModelRegistryConfig {
        providers: [(handle, connection)].into(),
        models: [("fixture".into(), recipe)].into(),
        skip_auto_discover: true,
    }
    .parse_model_groups(&Default::default(), Default::default())
    .unwrap();
    (provider.model, groups["fixture"].main.clone())
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    server
}

async fn invoke(provider: Arc<dyn ModelProvider>, model: ModelConfig, stream: bool) {
    let mut request = ModelRequest::new("hello");
    request.system = "system".into();
    let tokens = stream.then(|| tokio::sync::mpsc::channel(16).0);
    // Assert the captured request separately: errors before dispatch must not
    // masquerade as the deliberate HTTP failure used by this fixture.
    assert!(
        RigModelAdapter::new(provider, model)
            .generate(request, tokens)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn signed_seeds_and_explicit_empty_stop_lists_keep_their_wire_values() {
    for (brand, api, prefix) in [
        ("openai", "completions", ""),
        ("groq", "completions", ""),
        ("openrouter", "completions", ""),
        ("ollama", "ollama", "/options"),
    ] {
        let server = server().await;
        for (seed, stop) in [
            (-7_i64, json!([])),
            (0, json!(["END"])),
            (i64::MAX, json!(["END", "DONE"])),
        ] {
            let (provider, model) = configured(
                brand,
                &server.uri(),
                json!({
                    "model":"private/manual-model", "api":api, "seed":seed,
                    "stop":stop, "top_p":0.75, "max_tokens":123
                }),
            );
            for stream in [false, true] {
                invoke(provider.clone(), model.clone(), stream).await;
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2, "{brand}: both calls must reach the wire");
            for request in requests {
                let body: Value = request.body_json().unwrap();
                assert_eq!(
                    body.pointer(&format!("{prefix}/seed")),
                    Some(&json!(seed)),
                    "{brand}: {body}"
                );
                assert_eq!(
                    body.pointer(&format!("{prefix}/stop")),
                    Some(&stop),
                    "{brand}: {body}"
                );
                assert_eq!(
                    body.pointer(&format!("{prefix}/top_p")),
                    Some(&json!(0.75)),
                    "{brand}: {body}"
                );
            }
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(502))
                .mount(&server)
                .await;
        }
    }
}

#[tokio::test]
async fn catalog_facts_do_not_newly_reject_previously_forwarded_settings() {
    // These combinations were sent at baseline a0ab8cf even when Rig's catalog
    // describes a stricter model rule. Moving fields into typed options must
    // preserve the existing provider-owned validation contract.
    for (brand, settings, expected) in [
        (
            "openai",
            json!({"model":"gpt-5.2","api":"completions","temperature":0.4,"top_p":0.75,"reasoning_effort":"medium"}),
            json!({"temperature":0.4,"top_p":0.75,"reasoning_effort":"medium"}),
        ),
        (
            "openai",
            json!({"model":"gpt-5.2","api":"responses","temperature":0.4,"top_p":0.75,"reasoning_effort":"future-effort"}),
            json!({"temperature":0.4,"top_p":0.75,"reasoning":{"effort":"future-effort"}}),
        ),
        (
            "anthropic",
            json!({"model":"claude-sonnet-4-5","temperature":0.4,"top_p":0.75,"thinking":{"type":"enabled","budget_tokens":100}}),
            json!({"temperature":0.4,"top_p":0.75,"thinking":{"type":"enabled","budget_tokens":100}}),
        ),
        (
            "google",
            json!({"model":"gemini-2.5-pro","temperature":0.4,"top_p":0.75,"thinking_config":{"thinking_budget":0,"include_thoughts":false}}),
            json!({"generationConfig":{"maxOutputTokens":123,"temperature":0.4,"topP":0.75,"thinkingConfig":{"thinkingBudget":0,"includeThoughts":false}}}),
        ),
    ] {
        let server = server().await;
        let (provider, model) = configured(brand, &server.uri(), settings);
        for stream in [false, true] {
            invoke(provider.clone(), model.clone(), stream).await;
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "{brand}: local catalog checks must not replace provider validation"
        );
        for request in requests {
            let body: Value = request.body_json().unwrap();
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&body[key], value, "{brand}: {body}");
            }
        }
    }
}

#[tokio::test]
async fn provider_extensions_keep_zero_false_empty_and_large_integer_values() {
    for (brand, settings, expected) in [
        (
            "google",
            json!({"top_k":u32::MAX,"candidate_count":1,"stop_sequences":[],"thinking_config":{"thinking_budget":0,"include_thoughts":false}}),
            json!({"generationConfig":{"maxOutputTokens":123,"topK":u32::MAX,"candidateCount":1,"stopSequences":[],"thinkingConfig":{"thinkingBudget":0,"includeThoughts":false}}}),
        ),
        (
            "anthropic",
            json!({"top_k":0,"stop_sequences":[],"thinking":{"type":"disabled"}}),
            json!({"top_k":0,"stop_sequences":[],"thinking":{"type":"disabled","budget_tokens":null},"cache_control":{"type":"ephemeral"}}),
        ),
        (
            "openai",
            json!({"api":"completions","frequency_penalty":0.0,"presence_penalty":-0.5,"logprobs":false,"top_logprobs":0,"min_p":0.0}),
            json!({"frequency_penalty":0.0,"presence_penalty":-0.5,"logprobs":false,"top_logprobs":0,"min_p":0.0}),
        ),
    ] {
        let server = server().await;
        let mut settings = settings.as_object().unwrap().clone();
        settings.insert("model".into(), json!("unknown-private-model"));
        let (provider, model) = configured(brand, &server.uri(), Value::Object(settings));
        for stream in [false, true] {
            invoke(provider.clone(), model.clone(), stream).await;
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "{brand}");
        for request in requests {
            let body: Value = request.body_json().unwrap();
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&body[key], value, "{brand}: {body}");
            }
        }
    }
}

#[tokio::test]
async fn concurrent_raw_option_overrides_do_not_mutate_the_shared_model_defaults() {
    let server = server().await;
    let settings = json!({"model":"unknown-private-model","api":"completions",
        "seed":7,"stop":["TYPED"],"top_p":0.75,"reasoning_effort":"medium",
        "max_tokens":321,"temperature":0.4});
    let (provider, defaults) = configured("openai", &server.uri(), settings);
    let mut custom = defaults.clone();
    custom.model_id = "custom-request".into();
    custom.request_settings.extra_params = json!({
        "seed":null,"stop":[],"top_p":null,"reasoning_effort":"future-effort",
        "max_completion_tokens":99,"temperature":null,
        "service_tier":"future-tier","parallel_tool_calls":false,
        "verbosity":"future-verbosity","prompt_cache_key":"private-cache-key"
    })
    .as_object()
    .unwrap()
    .clone();
    tokio::join!(
        invoke(provider.clone(), defaults.clone(), false),
        invoke(provider.clone(), custom, true),
    );
    invoke(provider, defaults, false).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests {
        let body: Value = request.body_json().unwrap();
        if body["model"] == "custom-request" {
            assert_eq!(body["seed"], Value::Null);
            assert!(body.get("seed").is_some());
            assert_eq!(body["stop"], json!([]));
            assert_eq!(body["top_p"], Value::Null);
            assert!(body.get("top_p").is_some());
            assert_eq!(body["temperature"], Value::Null);
            assert!(body.get("temperature").is_some());
            assert_eq!(body["max_completion_tokens"], 99);
            assert_eq!(body["reasoning_effort"], "future-effort");
            assert_eq!(body["service_tier"], "future-tier");
            assert_eq!(body["parallel_tool_calls"], false);
            assert_eq!(body["verbosity"], "future-verbosity");
            assert_eq!(body["prompt_cache_key"], "private-cache-key");
        } else {
            assert_eq!(body["seed"], 7);
            assert_eq!(body["stop"], json!(["TYPED"]));
            assert_eq!(body["top_p"], 0.75);
            assert_eq!(body["temperature"], 0.4);
            assert_eq!(body["max_completion_tokens"], 321);
            assert_eq!(body["reasoning_effort"], "medium");
            assert!(body.get("service_tier").is_none());
        }
    }
}
