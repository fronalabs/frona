use crate::inference::{
    config::ModelRegistryConfig,
    protocol::parameters::{WireParameters, merge_json},
    provider::{
        InferenceCounter, ModelConfig, ModelProvider,
        platform::{ProviderPlatform, build_provider},
    },
};
use crate::{
    chat::broadcast::BroadcastService,
    core::{
        Handle,
        config::{AdapterId, InferenceConfig, ModelGroupConfig, ModelProviderConfig},
    },
};
use rig_core::completion::{Message, ToolDefinition};
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn setup(brand: &str, endpoint: &str, settings: Value) -> (Arc<dyn ModelProvider>, ModelConfig) {
    let handle = Handle::try_new("account").unwrap();
    let connection = ModelProviderConfig {
        provider: Some(brand.into()),
        adapter: if brand == "custom-brand" {
            Some(AdapterId::Openai)
        } else {
            None
        },
        api_key: Some("fixture-key".into()),
        base_url: Some(endpoint.into()),
        ..Default::default()
    };
    let mut config = json!({"provider":"account","model":"test-model"});
    merge_json(&mut config, settings);
    let config: ModelGroupConfig = serde_json::from_value(config).unwrap();
    let resolved = ProviderPlatform::resolve(&handle, &connection).unwrap();
    let provider = build_provider(
        &resolved,
        &connection,
        &InferenceCounter::new(BroadcastService::new()),
    )
    .unwrap();
    let groups = ModelRegistryConfig {
        providers: [(handle, connection)].into(),
        models: [("test".into(), config)].into(),
        skip_auto_discover: true,
    }
    .parse_model_groups(&InferenceConfig::default(), Default::default())
    .unwrap();
    (provider, groups["test"].main.clone())
}

async fn capture(provider: &dyn ModelProvider, model: &ModelConfig, stream: bool) {
    // A deliberate HTTP error lets every native serializer be exercised without
    // inventing a shared response dialect. The server still captures the body.
    let max_tokens = model.request_settings.max_tokens.or(Some(123));
    let temperature = model.request_settings.temperature.or(Some(0.2));
    let result = if stream {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        provider
            .stream_inference(
                model,
                "system",
                vec![Message::user("hello")],
                vec![],
                tx,
                max_tokens,
                temperature,
            )
            .await
    } else {
        provider
            .inference(
                model,
                "system",
                vec![Message::user("hello")],
                vec![],
                max_tokens,
                temperature,
            )
            .await
    };
    assert!(result.is_err(), "fixture intentionally returns HTTP 502");
}

#[tokio::test]
async fn request_overrides_reach_normal_streaming_and_structured_wire_bodies() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let (provider, model) = setup(
        "openai",
        &server.uri(),
        json!({"max_tokens":8192,"temperature":0.7}),
    );
    let normal = provider.inference(
        &model,
        "system",
        vec![Message::user("title")],
        vec![],
        Some(1024),
        Some(0.0),
    );
    let structured = provider.structured_inference(
        &model,
        "system",
        vec![Message::user("title")],
        json!({"type":"object","properties":{"title":{"type":"string"}}}),
        Some(1024),
        Some(0.0),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let streaming = provider.stream_inference(
        &model,
        "system",
        vec![Message::user("title")],
        vec![],
        tx,
        Some(1024),
        Some(0.0),
    );
    let _ = tokio::join!(normal, structured, streaming);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests {
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["max_completion_tokens"], 1024);
        assert_eq!(body["temperature"], 0.0);
    }
    let _ = provider
        .inference(
            &model,
            "system",
            vec![Message::user("later")],
            vec![],
            None,
            None,
        )
        .await;
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests.last().unwrap().body_json().unwrap();
    assert_eq!(body["max_completion_tokens"], 8192);
    assert_eq!(body["temperature"], 0.7);
}

#[tokio::test]
async fn final_http_bodies_all_existing_adapters_normal_and_streaming() {
    for brand in [
        "openai",
        "anthropic",
        "ollama",
        "groq",
        "openrouter",
        "deepseek",
        "google",
        "cohere",
        "mistral",
        "perplexity",
        "togetherai",
        "xai",
        "hyperbolic",
        "moonshotai",
        "mira",
        "galadriel",
        "huggingface",
        "custom-brand",
        "kimi-for-coding",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let (provider, model) = setup(
            brand,
            &server.uri(),
            json!({"max_tokens":321,"temperature":0.3,"extra_params":{"fixture":{"nullable":null,"array":[1,"x",false]},"literal.dot":true}}),
        );
        for stream in [false, true] {
            capture(provider.as_ref(), &model, stream).await;
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "{brand}: both native serializers must send"
        );
        for request in requests {
            let body: Value = request.body_json().unwrap();
            assert_eq!(
                body["fixture"],
                json!({"nullable":null,"array":[1,"x",false]}),
                "{brand}: {body}"
            );
            assert_eq!(body["literal.dot"], true, "{brand}");
            let (max, temp) = match brand {
                "openai" | "galadriel" | "custom-brand" => {
                    ("/max_completion_tokens", "/temperature")
                }
                "google" => (
                    "/generationConfig/maxOutputTokens",
                    "/generationConfig/temperature",
                ),
                "ollama" => ("/options/num_predict", "/options/temperature"),
                _ => ("/max_tokens", "/temperature"),
            };
            assert_eq!(body.pointer(max), Some(&json!(321)), "{brand}: {body}");
            assert_eq!(body.pointer(temp), Some(&json!(0.3)), "{brand}: {body}");
            if brand == "xai" || brand == "huggingface" {
                assert!(
                    request.url.path().ends_with("/chat/completions"),
                    "{brand}: {}",
                    request.url
                );
                assert!(body["messages"].is_array());
                assert!(body.get("input").is_none());
            }
        }
    }
}

#[tokio::test]
async fn responses_typed_nested_merge_overrides_and_explicit_null_reach_http() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let (provider, model) = setup(
        "openai",
        &server.uri(),
        json!({"api":"responses","reasoning_effort":"high","max_tokens":123,"temperature":0.5,"extra_params":{"reasoning":{"summary":"detailed","effort":"low"},"temperature":null,"include":["reasoning.encrypted_content"]}}),
    );
    let plan = WireParameters::prepare(&model, None, None, false).unwrap();
    assert_eq!(plan.overrides.len(), 2);
    assert!(
        plan.overrides
            .iter()
            .any(|entry| entry.config_path.ends_with(".reasoning_effort")
                && entry.wire_path == ["reasoning", "effort"])
    );
    assert!(
        plan.overrides
            .iter()
            .any(|entry| entry.config_path.ends_with(".temperature")
                && entry.wire_path == ["temperature"])
    );
    for stream in [false, true] {
        capture(provider.as_ref(), &model, stream).await;
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let body: Value = request.body_json().unwrap();
        assert_eq!(
            body["reasoning"],
            json!({"effort":"low","summary":"detailed"})
        );
        assert_eq!(body["temperature"], Value::Null);
        assert!(body.get("temperature").is_some());
        assert_eq!(body["max_output_tokens"], 123);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("reasoning_effort").is_none());
    }
}

#[tokio::test]
async fn common_parameters_are_rig_inputs_not_post_serialization_overrides() {
    for brand in ["google", "ollama", "openai", "anthropic"] {
        let (_, model) = setup(
            brand,
            "http://localhost",
            json!({"max_tokens":321,"temperature":0.3}),
        );
        let plan = WireParameters::prepare(&model, Some(100), Some(0.1), false).unwrap();
        assert_eq!(plan.request.max_tokens, Some(100));
        assert_eq!(plan.request.temperature, Some(0.1));
        // There is no SDK body here: common fields must not be reconstructed by apply().
        let mut body = json!({});
        plan.apply(&mut body);
        assert_eq!(body, json!({}), "{brand}");
    }
}

#[tokio::test]
async fn ancestor_raw_override_keeps_detailed_diagnostics() {
    let (_, model) = setup(
        "google",
        "http://localhost",
        json!({
            "max_tokens":321,"temperature":0.3,"top_p":0.8,
            "extra_params":{"generationConfig":null}
        }),
    );
    let plan = WireParameters::prepare(&model, None, None, false).unwrap();
    assert_eq!(plan.overrides.len(), 3);
    for entry in &plan.overrides {
        assert_eq!(entry.wire_path[0], "generationConfig");
    }
    let mut body = json!({"generationConfig":{"maxOutputTokens":321,"temperature":0.3,"topP":0.8}});
    plan.apply(&mut body);
    assert_eq!(body, json!({"generationConfig":null}));
}

#[tokio::test]
async fn responses_preserve_native_aliases_and_unrecognized_reasoning_values() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let (provider, model) = setup(
        "openai",
        &server.uri(),
        json!({
            "api":"responses","max_tokens":100,"max_completion_tokens":200,
            "top_p":0.8,"top_logprobs":3,"reasoning_effort":"future-effort"
        }),
    );
    for stream in [false, true] {
        capture(provider.as_ref(), &model, stream).await;
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["max_output_tokens"], 200);
        assert_eq!(body["top_p"], 0.8);
        assert_eq!(body["top_logprobs"], 3);
        assert_eq!(body["reasoning"]["effort"], "future-effort");
    }
}

#[test]
fn recursive_merge_replaces_arrays_null_and_scalars() {
    let mut body = json!({"nested":{"a":1,"b":2},"array":[1,2],"object":{"a":1},"null":1});
    merge_json(
        &mut body,
        json!({"nested":{"a":3},"array":[9],"object":false,"null":null}),
    );
    assert_eq!(
        body,
        json!({"nested":{"a":3,"b":2},"array":[9],"object":false,"null":null})
    );
}

#[tokio::test]
async fn reserved_fields_and_structured_ancestors_reject_before_http() {
    let server = MockServer::start().await;
    for (brand, protocol, extras) in [
        (
            "openai",
            "completions",
            vec![
                json!({"model":null}),
                json!({"messages":[]}),
                json!({"tools":[]}),
                json!({"stream":false}),
                json!({"stream_options":null}),
            ],
        ),
        (
            "openai",
            "responses",
            vec![
                json!({"input":[]}),
                json!({"instructions":"bad"}),
                json!({"previous_response_id":"bad"}),
            ],
        ),
        (
            "google",
            "google-generate-content",
            vec![json!({"contents":[]}), json!({"systemInstruction":null})],
        ),
    ] {
        let (provider, model) = setup(brand, &server.uri(), json!({"api":protocol}));
        for extra in extras {
            let mut model = model.clone();
            model.request_settings.extra_params = extra.as_object().unwrap().clone();
            let err = provider
                .inference(&model, "sys", vec![Message::user("hi")], vec![], None, None)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("extra_params.") && err.contains("reserved"),
                "{err}"
            );
        }
    }
    for (brand, settings, extra) in [
        ("openai", json!({"api":"responses"}), json!({"text":null})),
        ("openai", json!({}), json!({"tool_choice":"none"})),
        ("google", json!({}), json!({"generationConfig":[]})),
        ("ollama", json!({}), json!({"format":{}})),
    ] {
        let (provider, mut model) = setup(brand, &server.uri(), settings);
        model.request_settings.extra_params = extra.as_object().unwrap().clone();
        let err = provider
            .structured_inference(
                &model,
                "sys",
                vec![Message::user("hi")],
                json!({"type":"object"}),
                None,
                None,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn native_typed_settings_survive_rig_serialization() {
    for (brand, settings, expected) in [
        (
            "google",
            json!({"top_p":0.8,"thinking_config":{"thinking_budget":100,"include_thoughts":true},"extra_params":{"generationConfig":{"thinkingConfig":{"fixture":true}}}}),
            json!({"topP":0.8,"thinkingConfig":{"thinkingBudget":100,"includeThoughts":true,"fixture":true}}),
        ),
        (
            "ollama",
            json!({"think":true,"num_ctx":8000,"top_k":3,"extra_params":{"options":{"top_k":4,"stop":["END"]}}}),
            json!({"num_ctx":8000,"top_k":4,"stop":["END"]}),
        ),
        (
            "anthropic",
            json!({"thinking":{"type":"enabled","budget_tokens":100},"extra_params":{"thinking":{"budget_tokens":200}}}),
            json!({"type":"enabled","budget_tokens":200}),
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let (provider, model) = setup(brand, &server.uri(), settings);
        for stream in [false, true] {
            capture(provider.as_ref(), &model, stream).await;
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            let body: Value = request.body_json().unwrap();
            let native = match brand {
                "google" => &body["generationConfig"],
                "ollama" => &body["options"],
                _ => &body["thinking"],
            };
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&native[key], value, "{brand}: {body}");
            }
            if brand == "google" {
                assert!(body.get("thinking_config").is_none());
            }
            if brand == "ollama" {
                assert_eq!(body["think"], true);
                assert!(body.get("num_ctx").is_none());
            }
        }
    }
}

#[test]
fn unsupported_settings_report_configuration_paths() {
    let registry = ModelRegistryConfig {
        providers: [(
            Handle::const_validated("account"),
            ModelProviderConfig {
                provider: Some("openai".into()),
                ..Default::default()
            },
        )]
        .into(),
        models: [(
            "test".into(),
            serde_json::from_value(
                json!({"provider":"account","model":"test","api":"responses","seed":42}),
            )
            .unwrap(),
        )]
        .into(),
        skip_auto_discover: true,
    };
    let error = registry
        .parse_model_groups(&InferenceConfig::default(), Default::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("models.test.seed"), "{error}");
    for settings in [
        json!({"thinking":{"type":"enabled","budget_toknes":42}}),
        json!({"thinking_config":{"thinking_budget":42,"include_thougths":true}}),
    ] {
        let mut config = json!({"provider":"account","model":"test"});
        merge_json(&mut config, settings);
        assert!(serde_json::from_value::<ModelGroupConfig>(config).is_err());
    }
}

#[tokio::test]
async fn structured_and_tool_calls_keep_envelope_and_custom_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"test","object":"chat.completion","created":1,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call1","type":"function","function":{"name":"submit","arguments":"{\"ok\":true}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}))).mount(&server).await;
    let (provider, model) = setup(
        "openai",
        &server.uri(),
        json!({"extra_params":{"service_tier":"priority"}}),
    );
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]});
    let result = provider
        .structured_inference(
            &model,
            "sys",
            vec![Message::user("hi")],
            schema.clone(),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"ok":true}));
    let tools = vec![ToolDefinition {
        name: "lookup".into(),
        description: "Lookup".into(),
        parameters: schema.clone(),
    }];
    provider
        .inference(&model, "sys", vec![Message::user("hi")], tools, None, None)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let structured: Value = requests[0].body_json().unwrap();
    assert_eq!(structured["tools"][0]["function"]["parameters"], schema);
    assert_eq!(structured["tools"][0]["function"]["name"], "submit");
    assert_eq!(structured["tool_choice"], "required");
    let normal: Value = requests[1].body_json().unwrap();
    assert_eq!(normal["tools"][0]["function"]["name"], "lookup");
    for body in [structured, normal] {
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["model"], "test-model");
    }
}

#[tokio::test]
async fn fallback_requests_use_their_own_settings() {
    use crate::db::repo::generic::SurrealRepo;
    use crate::inference::{
        provider::registry::ModelProviderRegistry,
        usage::{InferenceKind, UsageContext, UsageService},
    };
    use frona_model_catalog::{ModelCatalogSnapshot, ModelCatalogStore};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let handle = Handle::const_validated("account");
    let config=ModelRegistryConfig {
        providers:[(handle,ModelProviderConfig {provider:Some("openai".into()),api_key:Some("fixture".into()),base_url:Some(server.uri()),..Default::default()})].into(),
        models:[("test".into(),serde_json::from_value(json!({"provider":"account","model":"primary","max_tokens":111,"extra_params":{"fixture":"primary"},"retry":{"max_retries":0},"fallbacks":[{"provider":"account","model":"fallback","max_tokens":222,"extra_params":{"fixture":"fallback"}}]})).unwrap())].into(),
        skip_auto_discover:true,
    };
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let runtime = crate::inference::credential::runtime::RuntimeCredentials::new(
        config.providers.clone(),
        crate::credential::managed::test_support::credentials(db.clone(), "fixture").await,
        crate::inference::provider::InferenceCounter::new(BroadcastService::new()),
    );
    let groups = config
        .parse_model_groups_with_catalog(
            &InferenceConfig::default(),
            &ModelCatalogSnapshot::empty(),
            Arc::new(runtime.providers()),
        )
        .unwrap();
    let registry = ModelProviderRegistry::new(runtime.providers(), groups);
    let usage = UsageService::new(
        ModelCatalogStore::new(ModelCatalogSnapshot::empty()),
        SurrealRepo::new(db),
        BroadcastService::new(),
    );
    let ctx = UsageContext::new(
        InferenceKind::Title {
            agent_id: "test".into(),
            chat_id: "test".into(),
        },
        "user",
        "test",
    );
    let group = registry
        .resolve(&crate::inference::ModelRef("test".into()))
        .unwrap();
    let result = group
        .inference(crate::inference::ModelRequest {
            system_prompt: "sys",
            history: vec![Message::user("hi")],
            tools: vec![],
            usage_service: &usage,
            usage_context: &ctx,
            overrides: Default::default(),
        })
        .await;
    assert!(result.is_err());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for (request, expected, max) in [
        (&requests[0], "primary", 111),
        (&requests[1], "fallback", 222),
    ] {
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["model"], expected);
        assert_eq!(body["fixture"], expected);
        assert_eq!(body["max_completion_tokens"], max);
    }
}

#[tokio::test]
async fn concurrent_calls_on_cached_client_do_not_leak_custom_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_delay(std::time::Duration::from_millis(10)))
        .mount(&server)
        .await;
    let (provider, one) = setup(
        "openai",
        &server.uri(),
        json!({"extra_params":{"fixture":"one"}}),
    );
    let mut two = one.clone();
    two.model_id = "second".into();
    two.request_settings.extra_params = json!({"fixture":"two"}).as_object().unwrap().clone();
    tokio::join!(
        capture(provider.as_ref(), &one, false),
        capture(provider.as_ref(), &two, true)
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let body: Value = request.body_json().unwrap();
        assert_eq!(
            body["fixture"],
            if body["model"] == "second" {
                "two"
            } else {
                "one"
            }
        );
    }
}
// Local token construction is never authentication proof. The approved OAuth
// proof comes from the backend-owned token exchange, not Rig's authorize().
#[tokio::test]
async fn chatgpt_authorize_with_supplied_token_is_not_live_validation() {
    use crate::inference::provider::CredentialValidationError;
    use rig_core::providers::chatgpt;

    let server = wiremock::MockServer::start().await;
    let token_dir = tempfile::tempdir().unwrap();
    let client = chatgpt::Client::builder()
        .api_key(chatgpt::ChatGPTAuth::AccessToken {
            access_token: "deliberately-invalid-token".into(),
            account_id: Some("not-an-account".into()),
        })
        .base_url(&server.uri())
        .allow_device_flow(false)
        .token_dir(token_dir.path())
        .build()
        .unwrap();

    client.authorize().await.unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(std::fs::read_dir(token_dir.path()).unwrap().count(), 0);

    let provider = crate::inference::provider::RigProvider::new(
        client,
        crate::inference::provider::InferenceCounter::new(
            crate::chat::broadcast::BroadcastService::new(),
        ),
    );
    assert!(matches!(
        provider.validate_credentials().await,
        Err(CredentialValidationError::Unsupported)
    ));
    assert!(provider.list_models().await.is_err());
}
#[test]
fn derived_metadata_tracks_names_nesting_and_protocol_exceptions() {
    use super::parameters::{ParameterMetadata, WireDialect};

    #[allow(dead_code)]
    #[derive(serde::Serialize, frona_derive::ParameterMetadata)]
    #[serde(rename_all = "camelCase")]
    struct Nested {
        new_setting: u64,
        #[serde(rename = "type")]
        kind: String,
    }

    #[allow(dead_code)]
    #[derive(frona_derive::ParameterMetadata)]
    #[parameter(prefix = "options")]
    struct Settings {
        ordinary: u64,
        #[parameter(nested, path = "thinking")]
        child: Option<Nested>,
        #[parameter(root, responses = "reasoning.effort")]
        effort: String,
        #[parameter(responses = false)]
        legacy: bool,
        #[parameter(skip)]
        local_only: String,
    }

    let paths = |dialect| {
        Settings::parameter_bindings(dialect)
            .into_iter()
            .map(|binding| (binding.config_path, binding.wire_path.join(".")))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        paths(WireDialect::Responses),
        [
            ("ordinary", "options.ordinary"),
            ("child.newSetting", "options.thinking.newSetting"),
            ("child.type", "options.thinking.type"),
            ("effort", "reasoning.effort"),
        ]
        .map(|(config, wire)| (config.to_owned(), wire.to_owned()))
    );
    assert_eq!(
        paths(WireDialect::LegacyChat).last(),
        Some(&("legacy".to_owned(), "options.legacy".to_owned()))
    );
}

#[test]
fn common_metadata_preserves_protocol_paths_and_excludes_local_config() {
    use super::parameters::{ParameterMetadata, WireDialect};
    use crate::core::config::CommonModelFields;

    for (dialect, max_tokens, temperature) in [
        (
            WireDialect::Bedrock,
            "inferenceConfig.maxTokens",
            "inferenceConfig.temperature",
        ),
        (
            WireDialect::OpenAiChat,
            "max_completion_tokens",
            "temperature",
        ),
        (WireDialect::Responses, "max_output_tokens", "temperature"),
        (
            WireDialect::Gemini,
            "generationConfig.maxOutputTokens",
            "generationConfig.temperature",
        ),
        (
            WireDialect::Ollama,
            "options.num_predict",
            "options.temperature",
        ),
        (WireDialect::LegacyChat, "max_tokens", "temperature"),
        (WireDialect::Anthropic, "max_tokens", "temperature"),
        (WireDialect::Cohere, "max_tokens", "temperature"),
        (WireDialect::HuggingFace, "max_tokens", "temperature"),
    ] {
        let bindings = CommonModelFields::parameter_bindings(dialect);
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].config_path, "max_tokens");
        assert_eq!(bindings[0].wire_path.join("."), max_tokens);
        assert_eq!(bindings[1].config_path, "temperature");
        assert_eq!(bindings[1].wire_path.join("."), temperature);
    }
}
