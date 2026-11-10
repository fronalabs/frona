//! HTTP transport shared by adapters whose native listing is not exposed by Rig.
//! Endpoints, pagination, and response types belong to the individual adapters.

use reqwest::{
    Client, RequestBuilder,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::de::DeserializeOwned;

use crate::inference::{error::InferenceError, provider::CredentialValidationError};

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
    use crate::inference::provider::ModelProvider;
    use crate::inference::provider::ProviderModelList;
    use crate::inference::provider::ProviderModelListSource;
    use crate::inference::provider::platform::ProviderPlatform;
    use crate::inference::provider::platform::build_provider;
    use rig_core::model::ModelInfo;
    use std::sync::Arc;

    pub fn provider(brand: &str, endpoint: &str) -> Arc<dyn ModelProvider> {
        let config = ModelProviderConfig {
            provider: Some(brand.into()),
            base_url: Some(endpoint.into()),
            api_key: Some("fixture-key".into()),
            ..Default::default()
        };
        let connection =
            ProviderPlatform::resolve(&Handle::const_validated("account"), &config).unwrap();
        build_provider(&connection, &config, &InferenceCounter::default()).unwrap()
    }

    pub async fn models(provider: &dyn ModelProvider) -> Vec<ModelInfo> {
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
}
