//! HTTP transport shared by adapters whose native listing is not exposed by Rig.
//! Endpoints, pagination, and response types belong to the individual adapters.

use reqwest::{
    Client, RequestBuilder,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::de::DeserializeOwned;

use crate::inference::{error::InferenceError, provider::CredentialValidationError};

/// Bounded read-only probes never follow redirects to another account endpoint.
pub fn http_client() -> Result<rig_reqwest::ReqwestClient, InferenceError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map(rig_reqwest::ReqwestClient::from)
        .map_err(|_| InferenceError::ConfigError("Cannot build model discovery client".into()))
}

pub fn client(token: &str) -> Result<Client, InferenceError> {
    let mut headers = HeaderMap::new();
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| InferenceError::ConfigError("Invalid model discovery credential".into()))?;
    value.set_sensitive(true);
    headers.insert(AUTHORIZATION, value);
    Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build model discovery client".into()))
}

pub async fn get<T: DeserializeOwned>(
    request: RequestBuilder,
) -> Result<T, CredentialValidationError> {
    let response = request
        .send()
        .await
        .map_err(|_| failed("Model discovery request failed"))?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(CredentialValidationError::AuthenticationRejected),
        status => return Err(failed(format!("Model discovery returned HTTP {status}"))),
    }
    // Upstream bodies and URLs can contain credentials. Keep diagnostics local.
    response
        .json()
        .await
        .map_err(|_| failed("Invalid model discovery response"))
}

pub fn failed(message: impl Into<String>) -> CredentialValidationError {
    CredentialValidationError::Failed(message.into())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::Handle;
    use crate::config::ModelProviderConfig;
    use crate::inference::provider::InferenceCounter;
    use crate::inference::provider::ProviderAdmin;
    use crate::inference::provider::ProviderModelList;
    use crate::inference::provider::ProviderModelListSource;
    use crate::inference::provider::platform::ProviderPlatform;
    use crate::inference::provider::platform::build_provider;
    use rig_core::model::ModelInfo;
    use std::sync::Arc;

    pub fn provider(brand: &str, endpoint: &str) -> Arc<ProviderAdmin> {
        let config = ModelProviderConfig {
            provider: Some(brand.into()),
            base_url: Some(endpoint.into()),
            api_key: Some("fixture-key".into()),
            ..Default::default()
        };
        let connection =
            ProviderPlatform::resolve(&Handle::const_validated("account"), &config).unwrap();
        Arc::new(
            build_provider(&connection, &config, &InferenceCounter::default())
                .unwrap()
                .admin,
        )
    }

    pub async fn models(provider: &ProviderAdmin) -> Vec<ModelInfo> {
        let ProviderModelList::Listed { source, models } = provider.list_models().await.unwrap()
        else {
            panic!("expected live models")
        };
        assert_eq!(source, ProviderModelListSource::Account);
        models.data
    }

    #[tokio::test]
    async fn discovery_never_follows_redirects_or_reports_secret_response_bodies() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        for status in [302, 401, 403, 429, 500] {
            source.reset().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("location", target.uri())
                        .set_body_string("fixture-key"),
                )
                .mount(&source)
                .await;
            let error = get::<serde_json::Value>(client("fixture-key").unwrap().get(source.uri()))
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("fixture-key"));
        }
        assert!(target.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn compatible_directories_preserve_inventory_and_redact_errors() {
        use serde_json::json;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path},
        };
        for brand in ["huggingface", "hyperbolic"] {
            let server = MockServer::start().await;
            let provider = provider(brand, &format!("{}/proxy", server.uri()));
            for body in [
                json!({"data":[{"id":"org/new-chat","context_length":32000}]}),
                json!({"data":[]}),
            ] {
                server.reset().await;
                Mock::given(method("GET"))
                    .and(path("/proxy/v1/models"))
                    .and(header("authorization", "Bearer fixture-key"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
                    .expect(1)
                    .mount(&server)
                    .await;
                let listed = models(provider.as_ref()).await;
                if body["data"].as_array().unwrap().is_empty() {
                    assert!(listed.is_empty(), "{brand}");
                } else {
                    assert_eq!(listed.len(), 1, "{brand}");
                    assert_eq!(listed[0].id, "org/new-chat", "{brand}");
                }
                server.verify().await;
            }
            server.reset().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(403).set_body_string("fixture-key"))
                .mount(&server)
                .await;
            let error = provider.list_models().await.unwrap_err().to_string();
            assert!(!error.contains("fixture-key"), "{brand}");
        }
    }

    #[tokio::test]
    async fn unsupported_discovery_does_not_request_an_invented_endpoint() {
        let server = wiremock::MockServer::start().await;
        let provider = provider("perplexity", &server.uri());
        assert!(provider.list_models().await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
