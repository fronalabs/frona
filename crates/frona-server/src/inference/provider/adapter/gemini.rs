//! Gemini credential validation recognizes structured invalid-key responses.
use std::sync::Arc;

use rig_core::{client::ModelListingClient, providers::gemini};

use crate::{
    core::config::ModelProviderConfig,
    inference::{
        error::InferenceError,
        protocol::http::WireClient,
        provider::{
            CredentialValidationError, InferenceCounter, ModelProvider, RigProvider,
            platform::{ResolvedConnection, require_api_key},
        },
    },
};

pub(crate) fn build(
    connection: &ResolvedConnection,
    config: &ModelProviderConfig,
    counter: &InferenceCounter,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    let key = require_api_key(connection.handle.as_str(), config)?;
    let endpoint = connection
        .effective_base_url
        .as_deref()
        .expect("Gemini endpoint");
    let mut key_header = reqwest::header::HeaderValue::from_str(&key)
        .map_err(|_| InferenceError::ConfigError("Invalid Gemini API key".into()))?;
    key_header.set_sensitive(true);
    let validation_url = reqwest::Url::parse(&format!("{endpoint}/v1beta/models"))
        .map_err(|_| InferenceError::ConfigError("Invalid Gemini endpoint".into()))?;
    let validation_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build Gemini validation client".into()))?;
    let client = gemini::Client::builder()
        .api_key(&key)
        .base_url(endpoint)
        .http_client(WireClient::default())
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build Gemini client".into()))?;
    let discovery_client = client.clone();
    Ok(Arc::new(
        RigProvider::new(client, counter.clone())
            .with_wire_transport()
            .with_live_check(move || {
                let client = discovery_client.clone();
                async move {
                    client
                        .list_models()
                        .await
                        .map_err(CredentialValidationError::from)
                }
            })
            .with_validation_check(move || {
                let client = validation_client.clone();
                let url = validation_url.clone();
                let key_header = key_header.clone();
                async move {
                    let response = client
                        .get(url)
                        .header("x-goog-api-key", key_header)
                        .send()
                        .await
                        .map_err(|_| {
                            CredentialValidationError::Failed(
                                "Gemini credential request failed".into(),
                            )
                        })?;
                    let status = response.status().as_u16();
                    match status {
                        200 | 400 => {}
                        401 | 403 => return Err(CredentialValidationError::AuthenticationRejected),
                        status => {
                            return Err(CredentialValidationError::Failed(format!(
                                "Gemini returned HTTP {status}"
                            )));
                        }
                    }
                    let body: serde_json::Value = response.json().await.map_err(|_| {
                        CredentialValidationError::Failed("Invalid Gemini response".into())
                    })?;
                    if status == 400 {
                        // Google uses HTTP 400 for invalid keys as well as bad requests.
                        // Only the structured ErrorInfo reason establishes key rejection.
                        if body
                            .pointer("/error/details")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|details| {
                                details.iter().any(|detail| {
                                    detail["@type"] == "type.googleapis.com/google.rpc.ErrorInfo"
                                        && detail["reason"] == "API_KEY_INVALID"
                                })
                            })
                        {
                            return Err(CredentialValidationError::AuthenticationRejected);
                        }
                        return Err(CredentialValidationError::Failed(
                            "Gemini returned HTTP 400".into(),
                        ));
                    }
                    // Protobuf JSON can omit an empty repeated models field.
                    // Inventory parsing/pagination remains the native listing client's job.
                    if !body.is_object()
                        || body.get("error").is_some()
                        || body.get("models").is_some_and(|models| !models.is_array())
                    {
                        return Err(CredentialValidationError::Failed(
                            "Invalid Gemini model response".into(),
                        ));
                    }
                    Ok(())
                }
            }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::{
        chat::broadcast::BroadcastService,
        core::Handle,
        inference::{
            credential::store::{CredentialMethod, ProviderCredentials},
            provider::{
                ProviderModelList,
                platform::{ProviderPlatform, build_provider},
                validation::{
                    CandidateCredential, ProviderValidationError, ProviderValidationService,
                    ValidationCandidate,
                },
            },
        },
    };
    use serde_json::{Value, json};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    const KEY: &str = "private-gemini-key";

    fn invalid_key_error() -> Value {
        json!({"error": {
            "code": 400, "status": "INVALID_ARGUMENT", "message": format!("Invalid key: {KEY}"),
            "details": [
                {"@type":"type.googleapis.com/google.rpc.LocalizedMessage","message":"Invalid key"},
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com"}
            ]
        }})
    }

    fn counter() -> InferenceCounter {
        InferenceCounter::new(BroadcastService::new())
    }

    fn candidate(handle: &str, endpoint: &str, key: &str) -> ValidationCandidate {
        ValidationCandidate {
            handle: Handle::try_new(handle).unwrap(),
            config: ModelProviderConfig {
                provider: Some("google".into()),
                base_url: Some(endpoint.into()),
                ..Default::default()
            },
            credential: CandidateCredential::New {
                method: CredentialMethod::ApiKey,
                document: crate::credential::managed::integration::static_secret::document(
                    key.into(),
                ),
            },
        }
    }

    fn provider(endpoint: &str, key: &str) -> Result<Arc<dyn ModelProvider>, InferenceError> {
        let mut candidate = candidate("gemini-work", endpoint, key);
        candidate.config.api_key = Some(key.into());
        let connection = ProviderPlatform::resolve(&candidate.handle, &candidate.config)?;
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
        let store =
            crate::credential::managed::test_support::credentials(db, "fixture-secret").await;
        ValidationFixture {
            service: ProviderValidationService::new(store.clone(), counter())
                .with_timeout(Duration::from_secs(2)),
            store,
        }
    }

    async fn reject_controls(server: &MockServer) {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400).set_body_json(invalid_key_error()))
            .with_priority(10)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn named_connections_validate_with_header_keys_and_accept_empty_model_lists() {
        for body in [
            json!({}),
            json!({"models":[]}),
            json!({"models":[{"name":"models/gemini-fixture"}]}),
        ] {
            let server = MockServer::start().await;
            reject_controls(&server).await;
            Mock::given(method("GET"))
                .and(path("/proxy/v1beta/models"))
                .and(header("x-goog-api-key", KEY))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(2)
                .mount(&server)
                .await;
            let endpoint = format!("{}/proxy/", server.uri());
            let ValidationFixture { service, store } = validation_fixture().await;
            for handle in ["gemini-work", "gemini-personal"] {
                let proof = service
                    .validate(candidate(handle, &endpoint, KEY))
                    .await
                    .unwrap();
                assert_eq!(proof.pending.handle.as_str(), handle);
                assert_eq!(
                    proof.pending.binding.effective_endpoint.as_deref(),
                    Some(endpoint.trim_end_matches('/'))
                );
                assert!(proof.models.is_none());
                assert!(
                    store
                        .pending(proof.pending.validation_id)
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 4);
            for (index, request) in requests.iter().enumerate() {
                assert_eq!(request.url.path(), "/proxy/v1beta/models");
                assert!(request.url.query().is_none());
                assert!(!request.url.as_str().contains(KEY));
                assert!(request.body.is_empty());
                let key = request.headers["x-goog-api-key"].to_str().unwrap();
                if index % 2 == 0 {
                    assert!(key.starts_with("frona-validation-control-"));
                } else {
                    assert_eq!(key, KEY);
                }
            }
        }
    }

    #[tokio::test]
    async fn explicit_invalid_key_and_unauthorized_statuses_reject_credentials() {
        for status in [400, 401, 403] {
            let server = MockServer::start().await;
            reject_controls(&server).await;
            Mock::given(method("GET"))
                .and(path("/v1beta/models"))
                .and(header("x-goog-api-key", KEY))
                .respond_with(ResponseTemplate::new(status).set_body_json(invalid_key_error()))
                .mount(&server)
                .await;
            let ValidationFixture { service, store } = validation_fixture().await;
            let error = service
                .validate(candidate("gemini-work", &server.uri(), KEY))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ProviderValidationError::Rejected(_)),
                "{error:?}"
            );
            assert!(!format!("{error} {error:?}").contains(KEY));
            assert!(
                store
                    .pending_status(&Handle::const_validated("gemini-work"))
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn public_endpoint_accepting_control_key_never_stages_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[]})))
            .expect(1)
            .mount(&server)
            .await;
        let ValidationFixture { service, store } = validation_fixture().await;
        let error = service
            .validate(candidate("gemini-work", &server.uri(), KEY))
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderValidationError::Rejected(_)));
        assert!(error.to_string().contains("control credentials"));
        assert!(!format!("{error} {error:?}").contains(KEY));
        assert!(
            store
                .pending_status(&Handle::const_validated("gemini-work"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unrelated_and_malformed_errors_remain_operational_on_both_probes() {
        for fail_control in [false, true] {
            for response in [
                ResponseTemplate::new(400).set_body_json(json!({"error":{"message":format!("API_KEY_INVALID {KEY}")}})),
                ResponseTemplate::new(400).set_body_json(json!({"error":{"details":[{"@type":"type.googleapis.com/google.rpc.LocalizedMessage","reason":"API_KEY_INVALID"}]}})),
                ResponseTemplate::new(400).set_body_json(json!({"error":{"details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}})),
                ResponseTemplate::new(400).set_body_json(json!({"error":{"details":{"reason":"API_KEY_INVALID"}}})),
                ResponseTemplate::new(400).set_body_string(KEY),
                ResponseTemplate::new(500).set_body_json(invalid_key_error()),
                ResponseTemplate::new(503).set_body_string(KEY),
                ResponseTemplate::new(200).set_body_string(KEY),
                ResponseTemplate::new(200).set_body_json(json!([])),
                ResponseTemplate::new(200).set_body_json(json!({"models":KEY})),
                ResponseTemplate::new(200).set_body_json(invalid_key_error()),
            ] {
                let server = MockServer::start().await;
                reject_controls(&server).await;
                let mut mock = Mock::given(method("GET")).and(path("/v1beta/models"));
                if !fail_control { mock = mock.and(header("x-goog-api-key", KEY)); }
                mock.respond_with(response).mount(&server).await;
                let ValidationFixture { service, store } = validation_fixture().await;
                let error = service.validate(candidate("gemini-work", &server.uri(), KEY)).await.unwrap_err();
                assert!(matches!(error, ProviderValidationError::Failed(_)), "{error:?}");
                assert!(!format!("{error} {error:?}").contains(KEY));
                assert_eq!(server.received_requests().await.unwrap().len(), if fail_control { 1 } else { 2 });
                assert!(store.pending_status(&Handle::const_validated("gemini-work")).await.unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn validation_redirects_are_not_followed() {
        let redirected = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[]})))
            .expect(0)
            .mount(&redirected)
            .await;
        for location in [
            "/redirected".to_owned(),
            format!("{}/models", redirected.uri()),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/redirected"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .expect(0)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1beta/models"))
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
    async fn both_validation_probes_have_bounded_timeouts() {
        for stall_control in [false, true] {
            let server = MockServer::start().await;
            reject_controls(&server).await;
            let mut mock = Mock::given(method("GET")).and(path("/v1beta/models"));
            if !stall_control {
                mock = mock.and(header("x-goog-api-key", KEY));
            }
            mock.respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(1))
                    .set_body_json(json!({})),
            )
            .mount(&server)
            .await;
            let ValidationFixture { service, store } = validation_fixture().await;
            let error = service
                .with_timeout(Duration::from_millis(100))
                .validate(candidate("gemini-work", &server.uri(), KEY))
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
                    .pending_status(&Handle::const_validated("gemini-work"))
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn malformed_key_and_unreachable_endpoint_do_not_expose_secrets() {
        let key = format!("{KEY}\r\nInjected: value");
        let error = provider("https://generativelanguage.googleapis.com", &key)
            .err()
            .expect("invalid header");
        assert!(!format!("{error} {error:?}").contains(KEY));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let ValidationFixture { service, .. } = validation_fixture().await;
        let error = service
            .validate(candidate("gemini-work", &endpoint, KEY))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProviderValidationError::Failed(_)),
            "{error:?}"
        );
        assert!(!format!("{error} {error:?}").contains(KEY));
    }

    #[tokio::test]
    async fn native_model_listing_remains_independent_of_the_credential_probe() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/proxy/v1beta/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"models":[{"name":"models/gemini-fixture","displayName":"Fixture"}]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        // Native Rig listing is unchanged; the header-only guarantee is for the
        // dedicated validation probe, not a rewrite of the SDK's existing transport.
        let ProviderModelList::Listed { models, .. } =
            provider(&format!("{}/proxy", server.uri()), KEY)
                .unwrap()
                .list_models()
                .await
                .unwrap()
        else {
            panic!("expected native Gemini model listing")
        };
        assert_eq!(models.data[0].id, "gemini-fixture");
    }
}
