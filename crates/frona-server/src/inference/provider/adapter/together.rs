//! Together returns a native array, not an OpenAI data envelope.
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
use rig_core::{
    model::{Model, ModelList},
    providers::together,
};
use std::sync::Arc;

async fn list_models(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<ModelList, CredentialValidationError> {
    let models: Vec<Model> =
        super::discovery::get(client.get(format!("{}/v1/models", endpoint.trim_end_matches('/'))))
            .await?;
    Ok(ModelList::new(
        models
            .into_iter()
            .filter(|model| matches!(model.r#type.as_deref(), Some("chat" | "language") | None))
            .collect(),
    ))
}

pub(crate) fn build(
    connection: &ResolvedConnection,
    config: &ModelProviderConfig,
    counter: &InferenceCounter,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    let key = require_api_key(connection.handle.as_str(), config)?;
    let endpoint = connection
        .effective_base_url
        .clone()
        .expect("Together endpoint");
    let client = together::Client::builder()
        .api_key(&key)
        .base_url(&endpoint)
        .http_client(WireClient::default())
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build Together client".into()))?;
    let discovery = super::discovery::client(&key)?;
    Ok(Arc::new(
        RigProvider::new(client, counter.clone())
            .with_wire_transport()
            .with_live_check(move || {
                let discovery = discovery.clone();
                let endpoint = endpoint.clone();
                async move { list_models(&discovery, &endpoint).await }
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
    async fn native_array_lists_current_chat_models_at_the_configured_endpoint() {
        let server = MockServer::start().await;
        let provider = provider("togetherai", &format!("{}/proxy", server.uri()));
        for body in [
            json!([{"id":"org/new-chat","type":"chat","context_length":32000},{"id":"org/image","type":"image"}]),
            json!([]),
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
            if body.as_array().unwrap().is_empty() {
                assert!(listed.is_empty());
            } else {
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].id, "org/new-chat");
                assert_eq!(listed[0].context_length, Some(32000));
            }
            server.verify().await;
        }
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[]})))
            .mount(&server)
            .await;
        assert!(provider.list_models().await.is_err());
    }
}
