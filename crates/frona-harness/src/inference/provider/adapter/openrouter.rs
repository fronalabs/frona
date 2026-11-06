//! OpenRouter's model directory is public; credential validation uses `/key`.
use std::sync::Arc;

use rig_core::{client::ModelListingClient, providers::openrouter};

use crate::config::ModelProviderConfig;
use crate::inference::error::InferenceError;
use crate::inference::protocol::http::WireClient;
use crate::inference::provider::CredentialValidationError;
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
        .expect("OpenRouter endpoint");
    let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| InferenceError::ConfigError("Invalid OpenRouter API key".into()))?;
    authorization.set_sensitive(true);
    let validation_url = reqwest::Url::parse(&format!("{endpoint}/key"))
        .map_err(|_| InferenceError::ConfigError("Invalid OpenRouter endpoint".into()))?;
    let validation_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|_| {
            InferenceError::ConfigError("Cannot build OpenRouter validation client".into())
        })?;
    let client = openrouter::Client::builder()
        .api_key(&key)
        .base_url(endpoint)
        .http_client(WireClient::default())
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build OpenRouter client".into()))?;
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
                let authorization = authorization.clone();
                async move {
                    let response = client
                        .get(url)
                        .header(reqwest::header::AUTHORIZATION, authorization)
                        .send()
                        .await
                        .map_err(|_| {
                            CredentialValidationError::Failed(
                                "OpenRouter credential request failed".into(),
                            )
                        })?;
                    match response.status().as_u16() {
                        200 => {}
                        401 | 403 => return Err(CredentialValidationError::AuthenticationRejected),
                        status => {
                            return Err(CredentialValidationError::Failed(format!(
                                "OpenRouter returned HTTP {status}"
                            )));
                        }
                    }
                    let body: serde_json::Value = response.json().await.map_err(|_| {
                        CredentialValidationError::Failed("Invalid OpenRouter key response".into())
                    })?;
                    if !body.get("data").is_some_and(serde_json::Value::is_object) {
                        return Err(CredentialValidationError::Failed(
                            "Invalid OpenRouter key response".into(),
                        ));
                    }
                    Ok(())
                }
            }),
    ))
}
