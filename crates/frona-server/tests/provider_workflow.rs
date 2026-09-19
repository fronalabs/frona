//! Combined issue 025 administration -> save -> restart -> inference acceptance.
//! All credentials and HTTP endpoints are synthetic; no provider account needed.
mod helpers;

use frona::credential::managed::Candidate;
use std::{path::Path, sync::Arc};

use frona::{
    chat::broadcast::BroadcastService,
    core::{
        Handle,
        config::{ApiSurface, Config, ConfigService, ModelProviderConfig},
    },
    credential::key_rotation::KeyRotation,
    inference::{
        config::ModelRegistryConfig,
        credential::store::CredentialMethod,
        directory::models::ModelDirectoryService,
        provider::{
            InferenceCounter,
            platform::ProviderPlatform,
            service::{CredentialInput, DraftRequest, ModelProviderService, ValidateRequest},
            validation::{ProviderValidationService, binding},
        },
    },
};
use frona_model_catalog::CatalogSources;
use serde_json::{Value, json};
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

async fn administration(db: &Surreal<Db>, directory: &Path, secret: &str) -> ModelProviderService {
    let file = directory.join("config.yaml");
    let config: Config =
        serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap_or_else(|_| "{}".into()))
            .unwrap();
    let fixture = helpers::app_state::build(db).await;
    fixture
        .state
        .vault_service
        .sync_config_connections()
        .await
        .unwrap();
    let store = frona::inference::credential::store::ProviderCredentials::new(
        frona::credential::managed::ManagedVault::new(
            Arc::new(frona::db::repo::managed_vault::SurrealManagedVaultRepo::new(db.clone())),
            secret,
            frona::credential::managed::GLOBAL_CONNECTION_ID.into(),
        ),
        Arc::new(frona::credential::managed::resolver::ManagedResolver::new(
            frona::credential::managed::integration::registered(),
        )),
    );
    let counter = InferenceCounter::new(BroadcastService::new());
    let validator = ProviderValidationService::new(store.clone(), counter.clone());
    let mut loaded = ConfigService::load(&file).unwrap();
    loaded.config.auth.encryption_secret = secret.into();
    let config_service = ConfigService::new(loaded).unwrap();
    let runtime = Arc::new(
        frona::inference::credential::runtime::RuntimeCredentials::new(
            config.providers.clone(),
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        ),
    );
    let groups = ModelRegistryConfig {
        providers: config.providers.clone(),
        models: config.models.clone(),
        skip_auto_discover: true,
    }
    .parse_model_groups_with_catalog(
        &config.inference,
        &frona_model_catalog::ModelCatalogSnapshot::empty(),
        Arc::new(runtime.providers()),
    )
    .unwrap();
    let providers = runtime.providers();
    ModelProviderService::new(
        ModelDirectoryService::new(CatalogSources::load(&directory.join("metadata"))),
        config_service,
        store,
        validator,
        runtime,
        Arc::new(config),
        groups,
        providers,
    )
}

async fn server(name: &'static str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", format!("Bearer key-{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":[{"id":"listed","object":"model","created":0,"owned_by":"fixture"}]}),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(header("authorization", format!("Bearer key-{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"result","object":"chat.completion","created":0,"model":"manual-unlisted",
            "choices":[{"index":0,"message":{"role":"assistant","content":name},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}})))
        .mount(&server).await;
    server
}

fn connection(server: &MockServer) -> ModelProviderConfig {
    ModelProviderConfig {
        provider: Some("openai".into()),
        base_url: Some(server.uri()),
        ..Default::default()
    }
}

async fn infer(service: &ModelProviderService, expected: &str) {
    let group = service
        .resolve(&frona::inference::ModelRef::PRIMARY)
        .unwrap();
    let usage = helpers::test_metrics_ctx();
    let context = helpers::test_usage_ctx();
    let frona::inference::ModelResponse { content, usage } = group
        .inference(frona::inference::ModelRequest {
            system_prompt: "fixture",
            history: vec![],
            tools: vec![],
            usage_service: &usage,
            usage_context: &context,
            overrides: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(usage.input_tokens, 2);
    assert_eq!(
        frona::inference::provider::extract_text_from_choice(&content).unwrap(),
        expected
    );
}

#[test]
fn explicit_vertex_configuration_is_rejected_but_gemini_api_keys_compile() {
    let compiles = |value: Value| {
        let Ok(config) = serde_json::from_value::<Config>(value) else {
            return false;
        };
        ModelRegistryConfig {
            providers: config.providers,
            models: config.models,
            skip_auto_discover: true,
        }
        .parse_model_groups(&config.inference, Default::default())
        .is_ok()
    };
    for brand in ["google-vertex", "google-vertex-anthropic"] {
        assert!(!compiles(json!({
            "providers":{"account":{"provider":brand,"api_key":"fixture-key"}},
            "models":{"primary":{"provider":"account","model":"manual"}}
        })));
    }
    assert!(!compiles(json!({
        "providers":{"account":{"provider":"google-vertex","adapter":"vertex"}},
        "models":{"primary":{"provider":"account","model":"manual","api":"google-vertex-generate-content"}}
    })));
    assert!(compiles(json!({
        "providers":{"account":{"provider":"google","api_key":"fixture-key"}},
        "models":{"primary":{"provider":"account","model":"manual","api":"google-generate-content"}}
    })));
}

#[tokio::test]
async fn ambient_aws_draft_proof_describes_manual_models_and_rejects_binding_changes() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let service = administration(&db, directory.path(), "fixture-encryption").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/foundation-models"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let handle = Handle::const_validated("bedrock");
    let config = ModelProviderConfig {
        provider: Some("amazon-bedrock".into()),
        base_url: Some(server.uri()),
        aws_region: Some("us-east-1".into()),
        aws_profile: Some("fixture-profile".into()),
        ..Default::default()
    };
    let resolved = ProviderPlatform::resolve(&handle, &config).unwrap();
    // Seed the exact proof produced by successful ambient validation. This
    // exercises draft discovery without AWS credentials or a live account.
    let proof = service
        .store
        .stage(
            &handle,
            CredentialMethod::Aws,
            binding(&resolved, "ambient", &config),
            &Candidate::External,
        )
        .await
        .unwrap();
    let draft = || DraftRequest {
        manual_models: vec!["manual-fixture-model".into()],
        config: config.clone(),
        validation_id: proof.validation_id,
        method: CredentialMethod::Aws,
        source: "ambient".into(),
    };
    service
        .store
        .claim_draft(proof.validation_id, "admin")
        .await
        .unwrap();
    let listing = service
        .draft_models("admin", &handle, draft())
        .await
        .unwrap();
    assert_eq!(listing.credential_method, Some(CredentialMethod::Aws));
    // Failed live discovery must remain an error, while manual models still work.
    assert_eq!(listing.source, "live_error");
    assert_eq!(listing.directory_status, "live_error");
    assert!(listing.manual_entry);
    let model = listing
        .models
        .iter()
        .find(|model| model.id == "manual-fixture-model")
        .unwrap();
    let protocol = model
        .protocols
        .iter()
        .find(|protocol| protocol.api == ApiSurface::AmazonBedrockConverse)
        .unwrap();
    assert!(protocol.available);
    assert!(!protocol.settings.is_empty());
    assert!(service.store.vault().list().await.unwrap().is_empty());
    let (_, payload) = service
        .store
        .pending(proof.validation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(payload, Candidate::External));
    for changed in ["region", "profile", "endpoint", "source", "method"] {
        let mut request = draft();
        match changed {
            "region" => request.config.aws_region = Some("eu-west-1".into()),
            "profile" => request.config.aws_profile = Some("other-profile".into()),
            "endpoint" => request.config.base_url = Some("https://other.invalid".into()),
            "source" => request.source = "database".into(),
            "method" => request.method = CredentialMethod::ApiKey,
            _ => unreachable!(),
        }
        assert!(
            service
                .draft_models("admin", &handle, request)
                .await
                .is_err(),
            "changed {changed} must invalidate the proof"
        );
    }
}

#[tokio::test]
async fn setup_duplicate_connections_failed_draft_restart_and_rotation_preserve_exact_pair() {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    KeyRotation::check(&db, "fixture-encryption-old")
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let a = server("a").await;
    let b = server("b").await;
    let handle = Handle::const_validated("account");
    let second = Handle::const_validated("second-account");
    let service = administration(&db, directory.path(), "fixture-encryption-old").await;
    let mut configured = std::collections::HashMap::new();
    for (handle, server, key) in [(&handle, &a, "key-a"), (&second, &b, "key-b")] {
        let proof = service
            .validate(
                "admin",
                handle,
                ValidateRequest {
                    config: connection(server),
                    credential: CredentialInput::ApiKey {
                        api_key: key.into(),
                    },
                },
            )
            .await
            .unwrap();
        let credential = service
            .accept(
                "admin",
                handle,
                DraftRequest {
                    manual_models: vec![],
                    config: connection(server),
                    validation_id: proof.validation_id,
                    method: CredentialMethod::ApiKey,
                    source: "database".into(),
                },
            )
            .await
            .unwrap();
        let mut config = connection(server);
        config.credential_id = credential.credential_id;
        configured.insert(handle.clone(), config);
        assert!(!serde_json::to_string(&proof).unwrap().contains(key));
    }
    let raw = json!({"nested":{"is_set":true,"items":[null,1]},"literal.dot":false});
    let initial = json!({"providers":configured,
        "models":{"primary":{"provider":"account","model":"manual-unlisted","api":"completions","max_tokens":77,"temperature":0.2,"extra_params":raw,
            "fallbacks":[{"provider":"second-account","model":"manual-unlisted","api":"completions"}]}}});
    let saved = service.config_service.save(initial, None).await.unwrap();
    assert!(saved.restart_required);
    assert!(
        service
            .resolve(&frona::inference::ModelRef::PRIMARY)
            .is_err()
    );
    let active = administration(&db, directory.path(), "fixture-encryption-old").await;
    infer(&active, "a").await;

    // An invalid credential and a proof for the wrong endpoint must both fail.
    let failed = active
        .validate(
            "admin",
            &handle,
            ValidateRequest {
                config: connection(&b),
                credential: CredentialInput::ApiKey {
                    api_key: "wrong-key".into(),
                },
            },
        )
        .await;
    assert!(failed.is_err());
    let good = active
        .validate(
            "admin",
            &handle,
            ValidateRequest {
                config: connection(&b),
                credential: CredentialInput::ApiKey {
                    api_key: "key-b".into(),
                },
            },
        )
        .await
        .unwrap();
    let revision = active
        .config_service
        .persisted()
        .unwrap()
        .persisted_revision;
    let mismatch = active
        .accept(
            "admin",
            &handle,
            DraftRequest {
                manual_models: vec![],
                config: connection(&a),
                validation_id: good.validation_id,
                method: CredentialMethod::ApiKey,
                source: "database".into(),
            },
        )
        .await;
    assert!(mismatch.is_err());
    assert_eq!(
        active
            .config_service
            .persisted()
            .unwrap()
            .persisted_revision,
        revision
    );
    infer(&active, "a").await;

    let accepted = active
        .accept(
            "admin",
            &handle,
            DraftRequest {
                manual_models: vec![],
                config: connection(&b),
                validation_id: good.validation_id,
                method: CredentialMethod::ApiKey,
                source: "database".into(),
            },
        )
        .await
        .unwrap();
    let next = active
        .config_service
        .save(
            json!({"providers":{"account":{"base_url":b.uri(),"credential_id":accepted.credential_id}}}),
            Some(&revision),
        )
        .await
        .unwrap();
    assert!(next.restart_required);
    infer(&active, "a").await;
    assert!(
        active
            .config_service
            .save(json!({}), Some(&revision))
            .await
            .is_err()
    );
    let restarted = administration(&db, directory.path(), "fixture-encryption-old").await;
    infer(&restarted, "b").await;
    assert_eq!(
        restarted.active.models["primary"].common.extra_params,
        *raw.as_object().unwrap()
    );
    assert!(
        !std::fs::read_to_string(directory.path().join("config.yaml"))
            .unwrap()
            .contains("key-")
    );
    let public = serde_json::to_string(&restarted.list().await.unwrap()).unwrap();
    for key in ["key-a", "key-b", "wrong-key"] {
        assert!(!public.contains(key));
    }

    let report = KeyRotation::check(&db, "fixture-encryption-new")
        .await
        .unwrap()
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(report.all_succeeded());
    let rotated = administration(&db, directory.path(), "fixture-encryption-new").await;
    infer(&rotated, "b").await;

    // Authoring metadata can disappear without changing the selected runtime.
    rotated
        .directory
        .catalogs
        .models
        .swap(frona::inference::directory::defaults::defaults());
    let listing = rotated
        .active_models_with_manual(&handle, &["manual-unlisted".into()])
        .await
        .unwrap();
    assert!(
        listing
            .models
            .iter()
            .any(|model| model.id == "manual-unlisted")
    );
    infer(&rotated, "b").await;
    for request in a
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .chain(b.received_requests().await.unwrap())
    {
        if request.method.as_str() == "POST" {
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["model"], "manual-unlisted");
            assert_eq!(body["max_completion_tokens"], 77);
            assert_eq!(body["temperature"], 0.2);
            assert_eq!(body["nested"], json!({"is_set":true,"items":[null,1]}));
        }
    }
}
