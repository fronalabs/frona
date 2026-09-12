mod helpers;

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use frona::{
    agent::prompt::PromptLoader,
    chat::{
        compactor::{ChatCompactor, ChatSummarizer},
        message::models::{Message, MessageRole},
    },
    core::{
        Handle,
        config::{ApiSurface, Config, ModelProviderConfig},
        error::AppError,
        repository::Repository,
    },
    db::repo::{generic::SurrealRepo, messages::SurrealMessageRepo},
    inference::{
        config::ModelRegistryConfig,
        context::DEFAULT_CONTEXT_WINDOW,
        credential::store::CredentialMethod,
        directory::models::{inventory, normalize},
        provider::{
            ModelRef, ProviderModelList, ProviderModelListSource, platform::ProviderPlatform,
            service::ModelProviderService,
        },
    },
};
use frona_model_catalog::{
    ModelCatalogSnapshot,
    catalog::{Limit, ModelEntry},
};
use serde_json::json;

fn config(model: &str, window: Option<usize>) -> Config {
    serde_json::from_value(json!({
        "providers":{"openai-prod":{"provider":"openai","api_key":"fixture-key"}},
        "models":{"primary":{"provider":"openai-prod","model":model,"max_tokens":20,"context_window":window}}
    })).unwrap()
}

fn catalog() -> ModelCatalogSnapshot {
    let mut catalog = ModelCatalogSnapshot::empty();
    for (model, context, input, output) in [
        ("small", 240, None, 40),
        ("large", 1_000_000, Some(900_000), 64_000),
    ] {
        catalog.entries.insert(
            format!("openai/{model}"),
            ModelEntry {
                limit: Limit {
                    context,
                    input,
                    output,
                },
                ..Default::default()
            },
        );
    }
    catalog
}

async fn service(config: &Config, catalog: &ModelCatalogSnapshot) -> ModelProviderService {
    let groups = ModelRegistryConfig {
        providers: config.providers.clone(),
        models: config.models.clone(),
        skip_auto_discover: true,
    }
    .parse_model_groups_with_catalog(&config.inference, catalog, Default::default())
    .unwrap();
    helpers::test_model_service(Default::default(), groups).await
}

#[tokio::test]
async fn context_budget_preserves_override_catalog_and_unknown_model_precedence() {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    for (model, window, expected) in [
        ("small", None, 200),
        ("large", None, 900_000),
        ("small", Some(1234), 1234),
        ("unknown", None, DEFAULT_CONTEXT_WINDOW),
    ] {
        let config = config(model, window);
        let service = service(&config, &catalog()).await;
        let group = service.resolve(&ModelRef::PRIMARY).unwrap();
        assert_eq!(group.context_window, expected, "{model}");
        assert_eq!(group.main.provider_name(), "openai-prod");
    }
    let group = service(&config("small", None), &ModelCatalogSnapshot::empty())
        .await
        .resolve(&ModelRef::PRIMARY)
        .unwrap();
    assert_eq!(group.context_window, DEFAULT_CONTEXT_WINDOW);
}

#[tokio::test]
async fn legacy_provider_aliases_resolve_catalog_budgets() {
    for (handle, brand) in [
        ("gemini", "google"),
        ("together", "togetherai"),
        ("moonshot", "moonshotai"),
    ] {
        let config: Config = serde_json::from_value(json!({
            "providers":{handle:{"api_key":"fixture-key"}},
            "models":{"primary":{"provider":handle,"model":"legacy-model"}}
        }))
        .unwrap();
        let mut catalog = ModelCatalogSnapshot::empty();
        catalog.entries.insert(
            format!("{brand}/legacy-model"),
            ModelEntry {
                limit: Limit {
                    context: 2000,
                    input: Some(1500),
                    output: 500,
                },
                ..Default::default()
            },
        );
        assert_eq!(
            service(&config, &catalog)
                .await
                .resolve(&ModelRef::PRIMARY)
                .unwrap()
                .context_window,
            1500,
            "{handle}"
        );
    }
}

struct Summarizer;
#[async_trait]
impl ChatSummarizer for Summarizer {
    async fn summarize(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, AppError> {
        Ok("summary".into())
    }
}

#[tokio::test]
async fn compaction_uses_catalog_budget_and_respects_configured_override() {
    for (window, expected_compaction) in [(None, true), (Some(2000), false)] {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        frona::db::init::setup_schema(&db).await.unwrap();
        let messages: SurrealMessageRepo = SurrealRepo::new(db.clone());
        for index in 0..6 {
            let mut message = Message::builder("chat", MessageRole::User, "x".repeat(200)).build();
            message.created_at = chrono::Utc::now() - chrono::Duration::minutes(10)
                + chrono::Duration::seconds(index);
            messages.create(&message).await.unwrap();
        }
        let compactor = ChatCompactor::new(
            SurrealRepo::new(db),
            messages,
            Arc::new(Summarizer),
            PromptLoader::new(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
            ),
        );
        let group = service(&config("small", window), &catalog())
            .await
            .resolve(&ModelRef::PRIMARY)
            .unwrap();
        let outcome = compactor
            .compact_chat(
                "user",
                "chat",
                "agent",
                "system",
                group.context_window,
                group.max_tokens.unwrap() as usize,
            )
            .await
            .unwrap();
        assert_eq!(outcome.compacted, expected_compaction);
    }
}

#[test]
fn live_model_limits_survive_inventory_and_override_stale_catalog_fields() {
    let handle = Handle::const_validated("openai-prod");
    let connection = ProviderPlatform::resolve(
        &handle,
        &ModelProviderConfig {
            provider: Some("openai".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let models=serde_json::from_value(json!({"data":[
        {"id":"small","name":"Live small","context_length":500,"max_output_tokens":60,"description":"Live description"},
        {"id":"new","context_length":2000,"max_output_tokens":100},
        {"id":"large"}
    ]})).unwrap();
    let listing = normalize(
        &connection,
        Some(CredentialMethod::ApiKey),
        &[ApiSurface::Completions],
        Default::default(),
        &[],
        inventory(ProviderModelList::Listed {
            source: ProviderModelListSource::Account,
            models,
        }),
        &catalog(),
    );
    let rows: HashMap<_, _> = listing
        .models
        .iter()
        .map(|row| (row.id.as_str(), row))
        .collect();
    assert_eq!(rows["small"].context_window, Some(500));
    assert_eq!(rows["small"].max_tokens, Some(60));
    assert_eq!(
        rows["small"].description.as_deref(),
        Some("Live description")
    );
    assert_eq!(rows["new"].context_window, Some(2000));
    assert_eq!(rows["new"].max_tokens, Some(100));
    assert_eq!(rows["large"].context_window, Some(1_000_000));
    assert_eq!(rows["large"].max_tokens, Some(64_000));
}
