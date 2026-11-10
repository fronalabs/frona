use crate::core::config::ModelProviderConfig;
use frona_harness::inference::{
    error::InferenceError,
    provider::{CredentialValidationError, InferenceCounter, ProviderConnection},
};

use std::time::Duration;

use crate::{
    chat::broadcast::BroadcastService,
    core::Handle,
    inference::{
        credential::store::ProviderCredentials,
        provider::{
            build_provider,
            validation::{
                CandidateCredential, ProviderValidationError, ProviderValidationService,
                ValidationCandidate,
            },
        },
    },
};
use frona_harness::{config::CredentialMethod, inference::provider::ProviderModelList};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

const KEY: &str = "private-openrouter-key";

fn counter() -> InferenceCounter {
    InferenceCounter::new(BroadcastService::new())
}

fn candidate(handle: &str, endpoint: &str, key: &str) -> ValidationCandidate {
    ValidationCandidate {
        handle: Handle::try_new(handle).unwrap(),
        config: ModelProviderConfig {
            provider: Some("openrouter".into()),
            base_url: Some(endpoint.into()),
            ..Default::default()
        },
        credential: CandidateCredential::New {
            method: CredentialMethod::ApiKey,
            document: crate::credential::managed::integration::static_secret::document(key.into()),
        },
    }
}

fn provider(endpoint: &str, key: &str) -> Result<ProviderConnection, InferenceError> {
    let mut candidate = candidate("router-work", endpoint, key);
    candidate.config.api_key = Some(key.into());
    let connection =
        crate::inference::provider::resolve_connection(&candidate.handle, &candidate.config)?;
    build_provider(&connection, &candidate.config, &counter())
}

struct ValidationFixture {
    service: ProviderValidationService,
    store: ProviderCredentials,
}

async fn validation_fixture() -> ValidationFixture {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let store = crate::credential::managed::test_support::credentials(db, "fixture-secret").await;
    ValidationFixture {
        service: ProviderValidationService::new(store.clone(), counter())
            .with_timeout(Duration::from_secs(2)),
        store,
    }
}

async fn reject_controls(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/key"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(server)
        .await;
}

#[tokio::test]
async fn named_connections_validate_current_key_and_list_models_independently() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/proxy/v1/key"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/proxy/v1/key"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"label":"fixture","limit":null}})),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/proxy/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":[{"id":"vendor/model","name":"Public model","created":0}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = format!("{}/proxy/v1/chat/completions/", server.uri());
    let ValidationFixture { service, store } = validation_fixture().await;
    for handle in ["router-work", "router-personal"] {
        let proof = service
            .validate(candidate(handle, &endpoint, KEY))
            .await
            .unwrap();
        assert_eq!(proof.pending.handle.as_str(), handle);
        assert_eq!(
            proof.pending.binding.effective_endpoint.as_deref(),
            Some(format!("{}/proxy/v1", server.uri()).as_str())
        );
        assert!(
            proof.models.is_none(),
            "authentication must not masquerade as model inventory"
        );
        assert!(
            store
                .pending(proof.pending.validation_id)
                .await
                .unwrap()
                .is_some()
        );
    }
    // Listing stays available even with a rejected key: the directory is public.
    let invalid = provider(&endpoint, "invalid-key").unwrap();
    assert!(matches!(
        invalid.admin.validate_credentials().await,
        Err(CredentialValidationError::AuthenticationRejected)
    ));
    let ProviderModelList::Listed { models, .. } = invalid.admin.list_models().await.unwrap()
    else {
        panic!("expected the native OpenRouter model list")
    };
    assert_eq!(models.data[0].id, "vendor/model");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 6);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/key"))
            .count(),
        5
    );
    for request in requests {
        assert!(request.url.query().is_none());
        assert!(!request.url.as_str().contains(KEY));
        assert!(request.body.is_empty());
        let authorization = request
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(authorization.starts_with("Bearer "));
    }
}

#[tokio::test]
async fn rejected_keys_and_control_accepting_servers_never_stage_credentials() {
    for accepts_control in [false, true] {
        let server = MockServer::start().await;
        let response = if accepts_control {
            ResponseTemplate::new(200).set_body_json(json!({"data":{"label":"public"}}))
        } else {
            ResponseTemplate::new(401).set_body_string(KEY)
        };
        Mock::given(method("GET"))
            .and(path("/key"))
            .respond_with(response)
            .mount(&server)
            .await;
        let ValidationFixture { service, store } = validation_fixture().await;
        let error = service
            .validate(candidate("router-work", &server.uri(), KEY))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProviderValidationError::Rejected(_)),
            "{error:?}"
        );
        let diagnostic = format!("{error} {error:?}");
        assert!(!diagnostic.contains(KEY));
        if accepts_control {
            assert!(diagnostic.contains("control credentials"));
        }
        assert!(
            store
                .pending_status(&Handle::const_validated("router-work"))
                .await
                .unwrap()
                .is_empty()
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), if accepts_control { 1 } else { 2 });
        assert!(
            requests[0].headers["authorization"]
                .to_str()
                .unwrap()
                .starts_with("Bearer frona-validation-control-")
        );
        if !accepts_control {
            assert_eq!(
                requests[1].headers["authorization"],
                format!("Bearer {KEY}")
            );
        }
    }
}

#[tokio::test]
async fn operational_failures_remain_distinct_on_control_and_supplied_key_probes() {
    for fail_control in [false, true] {
        for status in [429, 500, 503] {
            let server = MockServer::start().await;
            reject_controls(&server).await;
            let mut mock = Mock::given(method("GET")).and(path("/key"));
            if !fail_control {
                mock = mock.and(header("authorization", format!("Bearer {KEY}")));
            }
            mock.respond_with(ResponseTemplate::new(status).set_body_string(KEY))
                .mount(&server)
                .await;
            let ValidationFixture { service, store } = validation_fixture().await;
            let error = service
                .validate(candidate("router-work", &server.uri(), KEY))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ProviderValidationError::Failed(_)),
                "{error:?}"
            );
            assert!(error.to_string().contains(&format!("HTTP {status}")));
            assert!(!format!("{error} {error:?}").contains(KEY));
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                if fail_control { 1 } else { 2 }
            );
            assert!(
                store
                    .pending_status(&Handle::const_validated("router-work"))
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn redirects_never_receive_authorization_or_establish_validation() {
    let redirected = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{}})))
        .expect(0)
        .mount(&redirected)
        .await;
    for location in [
        "/redirected".to_owned(),
        format!("{}/key", redirected.uri()),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{}})))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/key"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", location)
                    .set_body_string(KEY),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = provider(&server.uri(), KEY)
            .unwrap()
            .admin
            .validate_credentials()
            .await
            .unwrap_err();
        assert!(matches!(error, CredentialValidationError::Failed(_)));
        assert!(error.to_string().contains("HTTP 307"));
        assert!(!format!("{error} {error:?}").contains(KEY));
    }
    assert!(redirected.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn timeout_is_bounded_for_both_control_and_supplied_keys() {
    for stall_control in [false, true] {
        let server = MockServer::start().await;
        reject_controls(&server).await;
        let mut mock = Mock::given(method("GET")).and(path("/key"));
        if !stall_control {
            mock = mock.and(header("authorization", format!("Bearer {KEY}")));
        }
        mock.respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(json!({"data":{}})),
        )
        .mount(&server)
        .await;
        let ValidationFixture { service, store } = validation_fixture().await;
        let error = service
            .with_timeout(Duration::from_millis(100))
            .validate(candidate("router-work", &server.uri(), KEY))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProviderValidationError::TimedOut(100)),
            "{error:?}"
        );
        assert!(!format!("{error} {error:?}").contains(KEY));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            if stall_control { 1 } else { 2 }
        );
        assert!(
            store
                .pending_status(&Handle::const_validated("router-work"))
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn malformed_responses_and_unreachable_endpoints_do_not_expose_secrets() {
    for response in [
        ResponseTemplate::new(200).set_body_string(KEY),
        ResponseTemplate::new(200).set_body_json(json!({"data":[KEY]})),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(response)
            .mount(&server)
            .await;
        let error = provider(&server.uri(), KEY)
            .unwrap()
            .admin
            .validate_credentials()
            .await
            .unwrap_err();
        assert!(matches!(error, CredentialValidationError::Failed(_)));
        assert!(!format!("{error} {error:?}").contains(KEY));
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let ValidationFixture { service, .. } = validation_fixture().await;
    let error = service
        .validate(candidate("router-work", &endpoint, KEY))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ProviderValidationError::Failed(_)),
        "{error:?}"
    );
    assert!(!format!("{error} {error:?}").contains(KEY));
}

#[tokio::test]
async fn malformed_authorization_header_does_not_expose_key() {
    let key = format!("{KEY}\r\nInjected: value");
    let error = provider("https://openrouter.ai/api/v1", &key)
        .err()
        .expect("invalid header");
    assert!(!format!("{error} {error:?}").contains(KEY));
}
