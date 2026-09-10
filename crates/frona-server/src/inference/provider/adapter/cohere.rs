//! Cohere's native paginated model directory uses names, not OpenAI model IDs.
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
    providers::cohere,
};
use serde::Deserialize;
use std::{collections::HashSet, sync::Arc};

#[derive(Deserialize)]
struct Page {
    models: Vec<Entry>,
    next_page_token: Option<String>,
}
#[derive(Deserialize)]
struct Entry {
    name: String,
    #[serde(default)]
    is_deprecated: bool,
    endpoints: Vec<String>,
    context_length: Option<u32>,
}

async fn list_models(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<ModelList, CredentialValidationError> {
    let url = format!("{}/v1/models", endpoint.trim_end_matches('/'));
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    loop {
        let mut request = client
            .get(&url)
            .query(&[("endpoint", "chat"), ("page_size", "1000")]);
        if let Some(cursor) = &cursor {
            request = request.query(&[("page_token", cursor)]);
        }
        let page: Page = super::discovery::get(request).await?;
        models.extend(
            page.models
                .into_iter()
                .filter(|entry| {
                    !entry.is_deprecated
                        && entry.endpoints.iter().any(|endpoint| endpoint == "chat")
                })
                .map(|entry| {
                    let mut model = Model::from_id(entry.name);
                    model.context_length = entry.context_length;
                    model
                }),
        );
        match page.next_page_token.filter(|token| !token.is_empty()) {
            Some(next) if seen.len() < 100 && seen.insert(next.clone()) => cursor = Some(next),
            Some(_) => return Err(super::discovery::failed("Invalid Cohere model pagination")),
            None => return Ok(ModelList::new(models)),
        }
    }
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
        .expect("Cohere endpoint");
    let client = cohere::Client::builder()
        .api_key(&key)
        .base_url(&endpoint)
        .http_client(WireClient::default())
        .build()
        .map_err(|_| InferenceError::ConfigError("Cannot build Cohere client".into()))?;
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
        matchers::{header, method, path, query_param},
    };

    #[tokio::test]
    async fn native_pages_keep_chat_models_and_encode_cursors() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/proxy/v1/models"))
            .and(header("authorization", "Bearer fixture-key"))
            .and(query_param("endpoint", "chat"))
            .respond_with(|request: &wiremock::Request| {
                let next = request.url.query_pairs().any(|(key, value)| key == "page_token" && value == "cursor&next=1");
                ResponseTemplate::new(200).set_body_json(if next { json!({"models":[{"name":"second-chat","endpoints":["chat"],"context_length":12000}]}) } else { json!({"models":[{"name":"first-chat","endpoints":["chat"]},{"name":"embed","endpoints":["embed"]},{"name":"retired","endpoints":["chat"],"is_deprecated":true}],"next_page_token":"cursor&next=1"}) })
            }).expect(2).mount(&server).await;
        let provider = provider("cohere", &format!("{}/proxy", server.uri()));
        let listed = models(provider.as_ref()).await;
        assert_eq!(
            listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["first-chat", "second-chat"]
        );
        assert_eq!(listed[1].context_length, Some(12000));
    }

    #[tokio::test]
    async fn empty_lists_are_authoritative_and_invalid_pages_do_not_become_suggestions() {
        let server = MockServer::start().await;
        let provider = provider("cohere", &server.uri());
        for (body, succeeds) in [
            (json!({"models":[]}), true),
            (json!({"models":[],"next_page_token":"repeated"}), false),
            (json!({"data":[]}), false),
        ] {
            server.reset().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
            if succeeds {
                assert!(models(provider.as_ref()).await.is_empty());
            } else {
                assert!(provider.list_models().await.is_err());
            }
        }
    }
}
