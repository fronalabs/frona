//! Gemini credential validation recognizes structured invalid-key responses.

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
    let client = rig_core::providers::gemini::GeminiConfig::new(key.clone())
        .with_base_url(endpoint)
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
            let key_header = key_header.clone();
            async move {
                let response = client
                    .get(url)
                    .header("x-goog-api-key", key_header)
                    .send()
                    .await
                    .map_err(|_| {
                        CredentialValidationError::Failed("Gemini credential request failed".into())
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
        }))
}
