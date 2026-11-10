//! Native inference with the provider's OpenAI-compatible model directory.
use crate::config::ModelProviderConfig;
use crate::inference::error::InferenceError;
use crate::inference::protocol::http::WireClient;
use crate::inference::provider::CredentialValidationError;
use crate::inference::provider::InferenceCounter;
use crate::inference::provider::ModelProvider;
use crate::inference::provider::RigProvider;
use crate::inference::provider::platform::ResolvedConnection;
use crate::inference::provider::platform::require_api_key;
use rig_core::providers::openai;
use std::sync::Arc;

pub(crate) fn build(
    connection: &ResolvedConnection,
    config: &ModelProviderConfig,
    counter: &InferenceCounter,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    let key = require_api_key(connection.handle.as_str(), config)?;
    let endpoint = connection
        .effective_base_url
        .as_deref()
        .expect("Hugging Face endpoint");
    let client = openai::OpenAIConfig::with_key(&openai::wire::HUGGINGFACE, key.clone())
        .with_base_url(endpoint)
        .with_route(openai::Route::Chat)
        .connect(WireClient::default());
    let discovery = openai::OpenAIConfig::new(key)
        .with_base_url(format!("{}/v1", endpoint.trim_end_matches('/')))
        .connect(WireClient::without_redirects().map_err(|_| {
            InferenceError::ConfigError("Cannot build model discovery transport".into())
        })?);
    Ok(Arc::new(
        RigProvider::new(
            move |id: &str| client.completion(id).erase(),
            counter.clone(),
        )
        .with_wire_transport()
        // The router model list is public and cannot validate a token.
        .with_validation_check(|| async { Err(CredentialValidationError::Unsupported) })
        .with_live_check(move || {
            let client = discovery.clone();
            async move {
                client
                    .list_models()
                    .await
                    .map_err(CredentialValidationError::from)
            }
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::discovery::tests::{models, provider};
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    #[tokio::test]
    async fn live_directory_uses_v1_models_and_preserves_empty_inventory() {
        let server = MockServer::start().await;
        let provider = provider("huggingface", &format!("{}/proxy", server.uri()));
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
                assert!(listed.is_empty());
            } else {
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].id, "org/new-chat");
            }
            server.verify().await;
        }
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("fixture-key"))
            .mount(&server)
            .await;
        let error = provider.list_models().await.unwrap_err().to_string();
        assert!(!error.contains("fixture-key"));
    }
}
