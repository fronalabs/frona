use crate::core::{config::ModelProviderConfig, handle::Handle};
use frona_harness::inference::provider::{
    InferenceCounter, ProviderConnection, adapter::azure::validate_deployment,
};

use crate::{
    chat::broadcast::BroadcastService,
    inference::{
        config::ModelRegistryConfig,
        provider::{
            build_provider,
            validation::{
                CandidateCredential, ProviderValidationService, ValidationCandidate, binding,
            },
        },
    },
};
use frona_harness::{
    config::{ApiSurface, CredentialMethod, InferenceConfig, ModelGroupConfig},
    inference::provider::ModelConfig,
};
use rig_core::completion::{AssistantContent, Message, ToolDefinition};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

fn counter() -> InferenceCounter {
    InferenceCounter::new(BroadcastService::new())
}

fn config(endpoint: &str) -> ModelProviderConfig {
    serde_json::from_value(json!({"provider":"azure","base_url":endpoint,"api_key":"fixture-key","azure_api_version":"2024-10-21"})).unwrap()
}

fn provider(config: &ModelProviderConfig) -> ProviderConnection {
    let resolved =
        crate::inference::provider::resolve_connection(&Handle::const_validated("account"), config)
            .unwrap();
    build_provider(&resolved, config, &counter()).unwrap()
}

fn model(config: &ModelProviderConfig, deployment: &str) -> ModelConfig {
    let group: ModelGroupConfig = serde_json::from_value(json!({"provider":"account","model":deployment,"api":"completions","max_tokens":123,"temperature":0.2,"top_p":0.7,"extra_params":{"temperature":null,"fixture":{"nested":[1,null]}}})).unwrap();
    ModelRegistryConfig {
        providers: [(Handle::const_validated("account"), config.clone())].into(),
        models: [("primary".into(), group)].into(),
        skip_auto_discover: true,
    }
    .parse_model_groups(&InferenceConfig::default(), Default::default())
    .unwrap()["primary"]
        .main
        .clone()
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: rig_core::message::ToolName::new("lookup").unwrap(),
        description: "Lookup".into(),
        parameters: json!({"type":"object","properties":{"q":{"type":"string"}}}),
        annotations: Default::default(),
    }]
}

fn response(tool: &str) -> Value {
    json!({"id":"fixture","object":"chat.completion","created":1,"model":"deployment","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call1","type":"function","function":{"name":tool,"arguments":"{\"q\":\"value\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}})
}

fn stream() -> String {
    let chunks = [
        json!({"id":"fixture","object":"chat.completion.chunk","created":1,"model":"deployment","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]},"finish_reason":null}]}),
        json!({"id":"fixture","object":"chat.completion.chunk","created":1,"model":"deployment","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"value\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"id":"fixture","object":"chat.completion.chunk","created":1,"model":"deployment","choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}),
    ];
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

#[tokio::test]
async fn deployment_paths_api_key_and_native_parameters_survive_normal_stream_and_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(stream(), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(response("lookup"))
            }
        })
        .mount(&server)
        .await;
    let config = config(&format!("{}/proxy/", server.uri()));
    let provider = provider(&config);
    for deployment in ["prod-gpt4-v2", "stage_deployment.1"] {
        let model = model(&config, deployment);
        for streaming in [false, true] {
            let output = if streaming {
                let (tx, _rx) = tokio::sync::mpsc::channel(16);
                crate::provider_fixture::invoke(
                    provider.model.clone(),
                    &model,
                    frona_harness::ModelRequest {
                        system: ("system").into(),
                        messages: vec![Message::user("hi")],
                        tools: tools(),
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
                        messages: vec![Message::user("hi")],
                        tools: tools(),
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
            assert!(output.content.iter().any(|content| matches!(content, AssistantContent::ToolCall(call) if call.function.name == "lookup" && serde_json::Value::Object(call.function.arguments.clone()) == json!({"q":"value"}))));
        }
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    for (index, request) in requests.iter().enumerate() {
        let deployment = if index < 2 {
            "prod-gpt4-v2"
        } else {
            "stage_deployment.1"
        };
        assert_eq!(
            request.url.path(),
            format!("/proxy/openai/deployments/{deployment}/chat/completions")
        );
        assert_eq!(request.url.query(), Some("api-version=2024-10-21"));
        assert_eq!(request.headers["api-key"], "fixture-key");
        assert!(!request.headers.contains_key("authorization"));
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["max_completion_tokens"], 123);
        assert_eq!(body["temperature"], Value::Null);
        assert_eq!(body["top_p"], 0.7);
        assert_eq!(body["fixture"], json!({"nested":[1,null]}));
        assert_eq!(body["tools"][0]["function"]["name"], "lookup");
    }
}

#[tokio::test]
async fn read_only_validation_is_not_deployment_inventory_and_errors_are_sanitized() {
    for (status, body, succeeds) in [
        (200, json!({"data":[]}), true),
        (200, json!({"data":[{"id":"base-model"}]}), true),
        (200, json!({"wrong":"fixture-key"}), false),
        (401, json!({"secret":"fixture-key"}), false),
        (403, json!({}), false),
        (500, json!({}), false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/openai/models"))
            .and(query_param("api-version", "2024-10-21"))
            .and(header("api-key", "fixture-key"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let provider = provider(&config(&server.uri()));
        let result = provider.admin.validate_credentials().await;
        assert_eq!(result.is_ok(), succeeds);
        if let Ok(check) = result {
            assert!(check.models.is_none());
        } else {
            assert!(!result.unwrap_err().to_string().contains("fixture-key"));
        }
        assert!(provider.admin.list_models().await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn structured_calls_keep_custom_data_and_reject_envelope_overrides() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response("submit")))
        .expect(1)
        .mount(&server)
        .await;
    let config = config(&server.uri());
    let provider = provider(&config);
    let mut model = model(&config, "structured-deployment");
    let value = crate::provider_fixture::invoke(
        provider.model.clone(),
        &model,
        frona_harness::ModelRequest {
            system: ("system").into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            schema: Some(
                json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}),
            ),
            max_tokens: None,
            temperature: None,
        },
        None,
    )
    .await
    .map(|output| output.structured.expect("validated structured output"))
    .unwrap();
    assert_eq!(value, json!({"q":"value"}));
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    assert_eq!(body["fixture"], json!({"nested":[1,null]}));
    assert_eq!(body["tools"][0]["function"]["name"], "submit");
    for extra in [
        json!({"model":"other"}),
        json!({"messages":[]}),
        json!({"tools":[]}),
        json!({"stream":false}),
    ] {
        model.request_settings.extra_params = extra.as_object().unwrap().clone();
        assert!(
            crate::provider_fixture::invoke(
                provider.model.clone(),
                &model,
                frona_harness::ModelRequest {
                    system: ("system").into(),
                    messages: vec![Message::user("hi")],
                    tools: vec![],
                    schema: None,
                    max_tokens: None,
                    temperature: None
                },
                None
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("reserved")
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn validation_proofs_bind_resource_and_api_version_without_activating_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("api-key", "fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[]})))
        .with_priority(1)
        .mount(&server)
        .await;
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let store =
        crate::credential::managed::test_support::credentials(db, "azure-fixture-secret").await;
    let service = ProviderValidationService::new(store.clone(), counter());
    let handle = Handle::const_validated("account");
    let mut config = config(&server.uri());
    config.api_key = None;
    let proof = service
        .validate(ValidationCandidate {
            handle: handle.clone(),
            config: config.clone(),
            credential: CandidateCredential::New {
                method: frona_harness::config::CredentialMethod::ApiKey,
                document: crate::credential::managed::integration::static_secret::document(
                    "fixture-key".into(),
                ),
            },
        })
        .await
        .unwrap();
    for change in [None, Some("url"), Some("version")] {
        let mut candidate = config.clone();
        match change {
            Some("url") => candidate.base_url = Some(format!("{}/other", server.uri())),
            Some(_) => {
                candidate
                    .attributes
                    .insert("azure_api_version".into(), json!("2025-01-01-preview"));
            }
            None => {}
        }
        let resolved = crate::inference::provider::resolve_connection(&handle, &candidate).unwrap();
        assert_eq!(
            service
                .resolve_proof(
                    proof.pending.validation_id,
                    &handle,
                    CredentialMethod::ApiKey,
                    &binding(&resolved, "database", &candidate)
                )
                .await
                .is_ok(),
            change.is_none()
        );
    }
    assert!(store.vault().list().await.unwrap().is_empty());
    assert!(
        service
            .validate(ValidationCandidate {
                handle,
                config,
                credential: CandidateCredential::New {
                    method: frona_harness::config::CredentialMethod::ApiKey,
                    document: crate::credential::managed::integration::static_secret::document(
                        "invalid".into()
                    )
                }
            })
            .await
            .is_err()
    );
}

#[test]
fn azure_setup_is_api_key_completions_only_and_configuration_round_trips() {
    let mut config = config("https://resource.openai.azure.com");
    let roundtrip: ModelProviderConfig =
        serde_yaml::from_str(&serde_yaml::to_string(&config).unwrap()).unwrap();
    assert_eq!(roundtrip.attributes, config.attributes);
    assert_eq!(
        model(&roundtrip, "custom-deployment-v2").model_id,
        "custom-deployment-v2"
    );
    let entries = crate::inference::directory::providers::available(
        &frona_model_catalog::catalog::ModelCatalogSnapshot::empty(),
    );
    let azure = entries.iter().find(|entry| entry.id == "azure").unwrap();
    assert_eq!(azure.api_surfaces, vec![ApiSurface::Completions]);
    assert_eq!(azure.auth_methods.len(), 1);
    assert_eq!(
        azure.auth_methods[0].credential_method,
        CredentialMethod::ApiKey
    );
    assert!(
        azure
            .fields
            .iter()
            .any(|field| field.id == "base_url" && field.required)
    );
    let handle = Handle::const_validated("account");
    config.azure_credential = Some("not-supported".into());
    assert!(crate::inference::provider::resolve_connection(&handle, &config).is_err());
    config.azure_credential = None;
    config
        .attributes
        .insert("azure_api_version".into(), json!("version&other=value"));
    assert!(crate::inference::provider::resolve_connection(&handle, &config).is_err());
    for invalid in ["../other", "name?api-version=other", "name/other", ".."] {
        assert!(validate_deployment(invalid).is_err());
    }
}

#[test]
fn catalog_routes_cannot_expand_azure_support_or_change_saved_deployments() {
    use crate::inference::directory::models::Inventory;
    use crate::inference::directory::models::normalize;
    use frona_model_catalog::catalog::ModelCatalogSnapshot;
    use frona_model_catalog::catalog::ModelEntry;
    use frona_model_catalog::catalog::ModelRoute;
    let config = config("https://resource.openai.azure.com");
    let connection = crate::inference::provider::resolve_connection(
        &Handle::const_validated("account"),
        &config,
    )
    .unwrap();
    let mut snapshot = ModelCatalogSnapshot::empty();
    snapshot
        .entries
        .insert("azure/suggestion".into(), ModelEntry::default());
    snapshot.entries.insert(
        "azure/claude".into(),
        ModelEntry {
            provider: Some(ModelRoute {
                npm: Some("@ai-sdk/anthropic".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    snapshot.entries.insert(
        "azure/gateway".into(),
        ModelEntry {
            provider: Some(ModelRoute {
                api: Some("https://other.example".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    for snapshot in [snapshot, ModelCatalogSnapshot::empty()] {
        let listing = normalize(
            &connection,
            Some(CredentialMethod::ApiKey),
            connection.protocols,
            [("saved-deployment".into(), vec!["primary".into()])].into(),
            &["manual-deployment".into()],
            Inventory::CatalogFallback,
            &snapshot,
        );
        assert!(listing.manual_entry);
        assert!(
            listing
                .models
                .iter()
                .any(|model| model.id == "saved-deployment")
        );
        assert!(
            listing
                .models
                .iter()
                .any(|model| model.id == "manual-deployment")
        );
        assert!(
            !listing
                .models
                .iter()
                .any(|model| matches!(model.id.as_str(), "claude" | "gateway"))
        );
        assert_eq!(
            model(&config, "saved-deployment").model_id,
            "saved-deployment"
        );
    }
    let group: ModelGroupConfig = serde_json::from_value(
        json!({"provider":"account","model":"saved-deployment","api":"responses"}),
    )
    .unwrap();
    assert!(
        crate::inference::config::ModelRegistryConfig {
            providers: [(
                crate::core::Handle::const_validated("account"),
                config.clone()
            )]
            .into(),
            models: [("primary".into(), group)].into(),
            skip_auto_discover: true,
        }
        .validate_model_groups()
        .is_err()
    );
}
