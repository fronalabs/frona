//! OpenRouter's model directory is public; credential validation uses `/key`.
use rig_core::providers::openai;

use crate::config::ModelProviderConfig;
use crate::inference::error::InferenceError;
use crate::inference::provider::CredentialValidationError;
use crate::inference::provider::ProviderAdmin;
use crate::inference::provider::platform::ResolvedConnection;
use crate::inference::provider::platform::require_api_key;

pub(crate) fn admin(
    connection: &ResolvedConnection,
    config: &ModelProviderConfig,
) -> Result<ProviderAdmin, InferenceError> {
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
    let client = openai::OpenAIConfig::with_key(&openai::wire::OPENROUTER, key)
        .with_base_url(endpoint)
        .with_route(openai::Route::Chat)
        .connect(super::discovery::http_client()?);
    let discovery_client = client.clone();
    Ok(ProviderAdmin::default()
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
        }))
}
