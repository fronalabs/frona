//! Deployment-scoped Azure OpenAI with explicit API-key authentication.

use crate::Handle;
use crate::config::ModelProviderConfig;
use crate::inference::{
    error::InferenceError,
    provider::{CredentialValidationError, ProviderAdmin, platform::ResolvedConnection},
};

pub const DEFAULT_API_VERSION: &str = "2024-10-21";

pub fn validate_attributes(
    handle: &Handle,
    config: &ModelProviderConfig,
) -> Result<(), InferenceError> {
    for (name, value) in &config.attributes {
        if name != "azure_api_version" {
            return Err(InferenceError::ConfigError(format!(
                "providers.{handle}.{name}: unknown Azure attribute"
            )));
        }
        if !value.as_str().is_some_and(|version| {
            !version.is_empty()
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }) {
            return Err(InferenceError::ConfigError(format!(
                "providers.{handle}.{name}: expected a nonempty API version containing letters, digits, or hyphens"
            )));
        }
    }
    Ok(())
}

pub fn validate_deployment(deployment: &str) -> Result<(), InferenceError> {
    if deployment.is_empty()
        || matches!(deployment, "." | "..")
        || !deployment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(InferenceError::ConfigError("Azure model must be an exact deployment name containing letters, digits, hyphens, underscores, or periods".into()));
    }
    Ok(())
}

pub fn admin(
    resolved: &ResolvedConnection,
    config: &ModelProviderConfig,
) -> Result<ProviderAdmin, InferenceError> {
    let endpoint = resolved.effective_base_url.as_deref().ok_or_else(|| {
        InferenceError::ConfigError("Azure requires base_url for the resource endpoint".into())
    })?;
    let key = config
        .api_key
        .as_ref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| InferenceError::ConfigError("Azure requires an API key".into()))?;
    let version = config
        .attributes
        .get("azure_api_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(DEFAULT_API_VERSION);
    let mut validation_url = reqwest::Url::parse(&format!("{endpoint}/openai/models"))
        .map_err(|_| InferenceError::ConfigError("Invalid Azure resource endpoint".into()))?;
    validation_url
        .query_pairs_mut()
        .append_pair("api-version", version);
    let mut headers = reqwest::header::HeaderMap::new();
    let mut key = reqwest::header::HeaderValue::from_str(key)
        .map_err(|_| InferenceError::ConfigError("Invalid Azure API key".into()))?;
    key.set_sensitive(true);
    headers.insert("api-key", key);
    let validation_client = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build Azure validation client".into()))?;
    Ok(ProviderAdmin::default().with_validation_check(move || {
        let client = validation_client.clone();
        let url = validation_url.clone();
        async move {
            let response = client.get(url).send().await.map_err(|_| {
                CredentialValidationError::Failed("Azure resource request failed".into())
            })?;
            match response.status().as_u16() {
                200 => {}
                401 | 403 => return Err(CredentialValidationError::AuthenticationRejected),
                status => {
                    return Err(CredentialValidationError::Failed(format!(
                        "Azure returned HTTP {status}"
                    )));
                }
            }
            let body: serde_json::Value = response.json().await.map_err(|_| {
                CredentialValidationError::Failed("Invalid Azure model response".into())
            })?;
            if !body.get("data").is_some_and(serde_json::Value::is_array) {
                return Err(CredentialValidationError::Failed(
                    "Invalid Azure model response".into(),
                ));
            }
            // These are base/fine-tuned model IDs, not callable deployment IDs.
            Ok(())
        }
    }))
}
