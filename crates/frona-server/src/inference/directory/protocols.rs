//! Translate upstream catalog labels into protocols supported by Frona.

use crate::core::config::OpenAiApi;
use frona_model_catalog::{
    ModelCatalogSnapshot,
    parameters::{ParameterCatalogSnapshot, ParameterModel},
};

pub fn openai_api_from_npm(npm: &str) -> Option<OpenAiApi> {
    match npm {
        "@ai-sdk/openai" => Some(OpenAiApi::Responses),
        "@ai-sdk/openai-compatible" => Some(OpenAiApi::ChatCompletions),
        _ => None,
    }
}

pub fn protocol_default(
    snapshot: &ModelCatalogSnapshot,
    provider: &str,
    model: &str,
) -> Option<OpenAiApi> {
    snapshot
        .protocol_hint(provider, model)
        .and_then(openai_api_from_npm)
        .or_else(|| {
            snapshot.entries.get(&format!("{provider}/{model}"))?;
            snapshot
                .providers
                .get(provider)?
                .npm
                .as_deref()
                .and_then(openai_api_from_npm)
        })
}

#[cfg(test)]
pub fn parameter_protocol(record: &ParameterModel) -> Option<crate::core::config::ApiSurface> {
    use crate::core::config::ApiSurface;
    match record.api_surface.as_str() {
        "openai-chat-completions" => Some(ApiSurface::Completions),
        "openai-responses" => Some(ApiSurface::Responses),
        other => serde_json::from_value(serde_json::Value::String(other.into())).ok(),
    }
}

pub fn exact_parameters<'a>(
    catalog: &'a ParameterCatalogSnapshot,
    provider: &str,
    access: &str,
    api: crate::core::config::ApiSurface,
    model: &str,
) -> Option<&'a ParameterModel> {
    use crate::core::config::ApiSurface;
    let serialized = serde_json::to_value(api).ok()?;
    let alias = serialized.as_str()?;
    let upstream = match api {
        ApiSurface::Completions => "openai-chat-completions",
        ApiSurface::Responses => "openai-responses",
        _ => alias,
    };
    catalog
        .exact(provider, access, upstream, model)
        .or_else(|| catalog.exact(provider, access, alias, model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ApiSurface;
    use serde_json::json;

    #[test]
    fn translates_model_and_provider_hints_without_guessing_for_manual_models() {
        let model = |npm: Option<&str>| {
            json!({
                "attachment": false,
                "reasoning": false,
                "tool_call": false,
                "open_weights": false,
                "limit": {"context": 8192, "output": 1024},
                "modalities": {"input": ["text"], "output": ["text"]},
                "provider": {"npm": npm}
            })
        };
        let snapshot = frona_model_catalog::loader::parse(
            &json!({"providers":{"openai":{"npm":"@ai-sdk/openai-compatible","models":{
                "responses":model(Some("@ai-sdk/openai")),
                "chat":model(None), "unknown":model(Some("unknown"))
            }}}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            protocol_default(&snapshot, "openai", "responses"),
            Some(OpenAiApi::Responses)
        );
        assert_eq!(
            protocol_default(&snapshot, "openai", "chat"),
            Some(OpenAiApi::ChatCompletions)
        );
        assert_eq!(
            protocol_default(&snapshot, "openai", "unknown"),
            Some(OpenAiApi::ChatCompletions)
        );
        assert_eq!(protocol_default(&snapshot, "openai", "manual"), None);
    }

    #[test]
    fn parameter_aliases_preserve_exact_model_and_authentication_matching() {
        for label in ["openai-chat-completions", "completions"] {
            let record: ParameterModel = serde_json::from_value(json!({
                "provider": "openai",
                "authType": "api_key",
                "apiSurface": label,
                "model": "display",
                "wireId": "exact-model",
                "params": []
            }))
            .unwrap();
            assert_eq!(parameter_protocol(&record), Some(ApiSurface::Completions));
            let catalog = ParameterCatalogSnapshot {
                models: vec![record],
                ..ParameterCatalogSnapshot::empty()
            };
            assert!(
                exact_parameters(
                    &catalog,
                    "openai",
                    "api_key",
                    ApiSurface::Completions,
                    "exact-model"
                )
                .is_some()
            );
            assert!(
                exact_parameters(
                    &catalog,
                    "openai",
                    "subscription",
                    ApiSurface::Completions,
                    "exact-model"
                )
                .is_none()
            );
            assert!(
                exact_parameters(
                    &catalog,
                    "openai",
                    "api_key",
                    ApiSurface::Completions,
                    "exact-model-dated"
                )
                .is_none()
            );
        }
    }
}
