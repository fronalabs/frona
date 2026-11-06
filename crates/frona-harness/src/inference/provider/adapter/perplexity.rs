//! Sonar has no documented model-list endpoint. Preserve manual model IDs;
//! do not send a speculative `/models` request or substitute catalog entries.
use std::sync::Arc;

use rig_core::providers::perplexity;

use crate::config::ModelProviderConfig;
use crate::inference::error::InferenceError;
use crate::inference::protocol::http::WireClient;
use crate::inference::provider::InferenceCounter;
use crate::inference::provider::ModelProvider;
use crate::inference::provider::RigProvider;
use crate::inference::provider::platform::ResolvedConnection;
use crate::inference::provider::platform::require_api_key;

pub(crate) fn build(
    connection: &ResolvedConnection,
    config: &ModelProviderConfig,
    counter: &InferenceCounter,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    let key = require_api_key(connection.handle.as_str(), config)?;
    let endpoint = connection
        .effective_base_url
        .as_deref()
        .expect("Perplexity endpoint");
    let client = perplexity::Client::builder()
        .api_key(&key)
        .base_url(endpoint)
        .http_client(WireClient::default())
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build Perplexity client".into()))?;
    Ok(Arc::new(
        RigProvider::new(client, counter.clone()).with_wire_transport(),
    ))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn unsupported_discovery_does_not_request_an_invented_endpoint() {
        let server = wiremock::MockServer::start().await;
        let provider = super::super::discovery::tests::provider("perplexity", &server.uri());
        assert!(provider.list_models().await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
