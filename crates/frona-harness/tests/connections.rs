//! Connection construction must preserve the chosen account and native protocol.
use std::sync::Arc;

use async_trait::async_trait;
use frona_harness::{
    Handle, ModelAdapter, ModelRequest,
    config::{AdapterId, ModelGroupConfig, ModelProviderConfig},
    inference::{
        InferenceError,
        config::ModelRegistryConfig,
        protocol::http::WireClient,
        provider::{
            InferenceCounter, ModelConfig, ModelProvider, ResolvedModel, RigModelAdapter,
            platform::{ProviderPlatform, build_provider},
        },
    },
};
use rig_core::{DynModel, operation::Completion};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn prepared(
    handle: &str,
    connection: ModelProviderConfig,
    settings: Value,
) -> (RigModelAdapter, ModelConfig) {
    let handle = Handle::try_new(handle).unwrap();
    let mut recipe = settings.as_object().unwrap().clone();
    recipe.insert("provider".into(), json!(handle.as_str()));
    let recipe: ModelGroupConfig = serde_json::from_value(Value::Object(recipe)).unwrap();
    let resolved = ProviderPlatform::resolve(&handle, &connection).unwrap();
    let provider = build_provider(&resolved, &connection, &InferenceCounter::default()).unwrap();
    let groups = ModelRegistryConfig {
        providers: [(handle, connection)].into(),
        models: [("fixture".into(), recipe)].into(),
        skip_auto_discover: true,
    }
    .parse_model_groups(&Default::default(), Default::default())
    .unwrap();
    let model = groups["fixture"].main.clone();
    (RigModelAdapter::new(provider.model, model.clone()), model)
}

async fn invoke(adapter: &RigModelAdapter, streaming: bool) {
    let mut request = ModelRequest::new("hello");
    request.system = "system".into();
    request.max_tokens = Some(321);
    request.temperature = Some(0.3);
    let tokens = streaming.then(|| tokio::sync::mpsc::channel(16).0);
    assert!(adapter.generate(request, tokens).await.is_err());
}

async fn rejecting_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn named_accounts_keep_independent_credentials_endpoints_and_explicit_routes() {
    let server = rejecting_server().await;
    for (handle, key, endpoint, api, model_id, path) in [
        (
            "production",
            "production-key",
            "/prod/v1",
            "responses",
            "private/manual-id:2026",
            "/prod/v1/responses",
        ),
        (
            "staging",
            "staging-key",
            "/stage/v1",
            "completions",
            "private/manual-id:2026",
            "/stage/v1/chat/completions",
        ),
    ] {
        let (adapter, model) = prepared(
            handle,
            ModelProviderConfig {
                provider: Some("openai".into()),
                api_key: Some(key.into()),
                base_url: Some(format!("{}{endpoint}", server.uri())),
                ..Default::default()
            },
            json!({"model":model_id,"api":api}),
        );
        assert_eq!(model.provider_handle.as_str(), handle);
        assert_eq!(model.catalog_provider, "openai");
        invoke(&adapter, false).await;
        let requests = server.received_requests().await.unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.url.path(), path);
        assert_eq!(request.headers["authorization"], format!("Bearer {key}"));
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["model"], model_id);
        if api == "responses" {
            assert!(body["input"].is_array());
            assert!(body.get("messages").is_none());
        } else {
            assert!(body["messages"].is_array());
            assert!(body.get("input").is_none());
        }
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

// These are the concrete constructors at the pre-consolidation baseline
// (691a44f). Comparing their real requests catches a registry dialect or route
// change independently of the new factory implementation.
struct BaselineModel(DynModel<Completion>);
#[async_trait]
impl ModelProvider for BaselineModel {
    async fn resolve(&self, _: &ModelConfig) -> Result<ResolvedModel, InferenceError> {
        Ok(ResolvedModel::new(self.0.clone()).with_wire_transport())
    }
}

#[tokio::test]
async fn cohere_and_generic_compatibility_recipes_match_previous_native_constructors() {
    for brand in ["cohere", "xai", "galadriel"] {
        let server = rejecting_server().await;
        let endpoint = format!("{}/gateway", server.uri());
        let (adapter, model) = prepared(
            "account",
            ModelProviderConfig {
                provider: Some(brand.into()),
                api_key: Some("fixture-key".into()),
                base_url: Some(endpoint.clone()),
                ..Default::default()
            },
            json!({"model":"unlisted-private-model"}),
        );
        let native = if brand == "cohere" {
            rig_core::providers::cohere::CohereConfig::new("fixture-key")
                .with_base_url(endpoint)
                .connect(WireClient::default())
                .completion("unlisted-private-model")
                .erase()
        } else {
            rig_core::providers::openai::OpenAIConfig::new("fixture-key")
                .with_base_url(endpoint)
                .with_route(rig_core::providers::openai::Route::Chat)
                .connect(WireClient::default())
                .chat("unlisted-private-model")
                .erase()
        };
        let baseline = RigModelAdapter::new(Arc::new(BaselineModel(native)), model);
        for streaming in [false, true] {
            invoke(&baseline, streaming).await;
            invoke(&adapter, streaming).await;
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4, "{brand}");
        for pair in requests.chunks_exact(2) {
            assert_eq!(pair[0].url, pair[1].url, "{brand}: route changed");
            assert_eq!(
                pair[0].body_json::<Value>().unwrap(),
                pair[1].body_json::<Value>().unwrap(),
                "{brand}: request changed"
            );
            assert_eq!(
                pair[0].headers["authorization"], pair[1].headers["authorization"],
                "{brand}: authentication changed"
            );
            assert_eq!(pair[1].headers["authorization"], "Bearer fixture-key");
            assert_eq!(
                pair[1].url.path(),
                if brand == "cohere" {
                    "/gateway/compatibility/v1/chat/completions"
                } else {
                    "/gateway/chat/completions"
                }
            );
        }
    }
}

#[tokio::test]
async fn azure_keeps_exact_deployment_version_and_api_key_authentication() {
    let server = rejecting_server().await;
    let (adapter, model) = prepared(
        "azure-production",
        ModelProviderConfig {
            provider: Some("azure".into()),
            api_key: Some("deployment-key".into()),
            base_url: Some(server.uri()),
            attributes: [("azure_api_version".into(), json!("2025-04-01-preview"))]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        json!({"model":"private_deployment.v2","api":"completions"}),
    );
    assert_eq!(model.model_id, "private_deployment.v2");
    for streaming in [false, true] {
        invoke(&adapter, streaming).await;
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(
            request.url.path(),
            "/openai/deployments/private_deployment.v2/chat/completions"
        );
        assert_eq!(request.url.query(), Some("api-version=2025-04-01-preview"));
        assert_eq!(request.headers["api-key"], "deployment-key");
        assert!(!request.headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn anonymous_ollama_and_generic_gateway_keep_their_distinct_protocols() {
    let server = rejecting_server().await;
    let (ollama, _) = prepared(
        "local",
        ModelProviderConfig {
            provider: Some("ollama".into()),
            base_url: Some(server.uri()),
            ..Default::default()
        },
        json!({"model":"custom-model:local","think":true,"num_ctx":8192}),
    );
    let (gateway, model) = prepared(
        "private",
        ModelProviderConfig {
            provider: Some("private-vendor".into()),
            adapter: Some(AdapterId::Openai),
            api_key: Some("gateway-key".into()),
            base_url: Some(format!("{}/private/v1", server.uri())),
            ..Default::default()
        },
        json!({"model":"organization/manual-model"}),
    );
    assert_eq!(model.provider_handle.as_str(), "private");
    for streaming in [false, true] {
        invoke(&ollama, streaming).await;
        invoke(&gateway, streaming).await;
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    for pair in requests.chunks_exact(2) {
        assert_eq!(pair[0].url.path(), "/api/chat");
        assert!(!pair[0].headers.contains_key("authorization"));
        let body: Value = pair[0].body_json().unwrap();
        assert_eq!(body["model"], "custom-model:local");
        assert_eq!(body["think"], true);
        assert_eq!(body["options"]["num_ctx"], 8192);
        assert_eq!(pair[1].url.path(), "/private/v1/chat/completions");
        assert_eq!(pair[1].headers["authorization"], "Bearer gateway-key");
        assert_eq!(
            pair[1].body_json::<Value>().unwrap()["model"],
            "organization/manual-model"
        );
    }
}
