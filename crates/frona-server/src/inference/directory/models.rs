#[cfg(test)]
use crate::credential::managed::Candidate;
use crate::inference::{
    credential::runtime::ResolvedProvider,
    provider::{
        ModelProvider, ProviderModelList, ProviderModelListSource, platform::ResolvedConnection,
    },
};
use crate::{
    core::{
        Handle,
        config::{ApiSurface, ModelGroupConfig},
    },
    inference::credential::store::CredentialMethod,
};
use chrono::Utc;
use frona_model_catalog::{
    ModelCatalogSnapshot,
    catalog::ModelEntry,
    sources::{CatalogSources, Source},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAvailability {
    Account,
    Catalog,
    Unverified,
    Configured,
    Recipe,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelCapabilities {
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    pub structured_output: Option<bool>,
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelProtocolInfo {
    pub api: ApiSurface,
    pub available: bool,
    pub settings: Vec<crate::inference::directory::settings::ModelSettingInfo>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub suggested_protocol: Option<ApiSurface>,
    pub protocols: Vec<ModelProtocolInfo>,
    pub availability: ModelAvailability,
    pub configured_in: Vec<String>,
    pub capabilities: ModelCapabilities,
    pub sources: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct InventoryModel {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum Inventory {
    Account(Vec<InventoryModel>),
    Recipe(Vec<InventoryModel>),
    CatalogFallback,
    Error,
}

#[derive(Serialize)]
pub struct ModelListing {
    pub connection: Handle,
    pub credential_method: Option<CredentialMethod>,
    pub access_mode: &'static str,
    pub source: &'static str,
    pub directory_status: &'static str,
    pub source_status: BTreeMap<String, Value>,
    pub manual_entry: bool,
    pub models: Vec<ProviderModelInfo>,
}

struct CachedInventory {
    identity: [u8; 32],
    expires: Instant,
    inventory: Inventory,
}

type InventorySlot = Arc<Mutex<Option<CachedInventory>>>;

#[derive(Clone)]
pub struct ModelDirectoryService {
    pub catalogs: CatalogSources,
    pub settings: crate::inference::directory::settings::SettingsDirectory,
    slots: Arc<Mutex<HashMap<Handle, InventorySlot>>>,
    ttl: Duration,
    timeout: Duration,
}

impl ModelDirectoryService {
    pub fn new(catalogs: CatalogSources) -> Self {
        Self {
            catalogs,
            settings: Default::default(),
            slots: Default::default(),
            ttl: Duration::from_secs(60),
            timeout: Duration::from_secs(10),
        }
    }

    pub(crate) async fn active_inventory(
        &self,
        handle: &Handle,
        resolved: &ResolvedProvider,
    ) -> Inventory {
        let slot = self
            .slots
            .lock()
            .await
            .entry(handle.clone())
            .or_default()
            .clone();
        let mut cached = slot.lock().await;
        if let Some(entry) = cached
            .as_ref()
            .filter(|entry| entry.identity == resolved.identity && entry.expires > Instant::now())
        {
            return entry.inventory.clone();
        }
        // A generation/routing/method change makes the old account list unusable.
        *cached = None;
        let inventory = self.draft_inventory(resolved.client.as_ref()).await;
        if matches!(inventory, Inventory::Account(_)) {
            *cached = Some(CachedInventory {
                identity: resolved.identity,
                expires: Instant::now() + self.ttl,
                inventory: inventory.clone(),
            });
        }
        inventory
    }

    pub async fn draft_inventory(&self, provider: &dyn ModelProvider) -> Inventory {
        // Drafts never enter shared inventory storage, even for the same handle.
        match tokio::time::timeout(self.timeout, provider.list_models()).await {
            Ok(Ok(list)) => inventory(list),
            _ => Inventory::Error,
        }
    }

    pub fn listing(
        &self,
        connection: &ResolvedConnection,
        method: Option<CredentialMethod>,
        eligible: &[ApiSurface],
        configured: BTreeMap<String, Vec<String>>,
        manual: &[String],
        inventory: Inventory,
    ) -> ModelListing {
        let snapshot = self.catalogs.models.current();
        let mut result = normalize(
            connection, method, eligible, configured, manual, inventory, &snapshot,
        );
        let parameters = self.catalogs.parameters.load_full();
        for model in &mut result.models {
            let mut exact_suggestion = None;
            for protocol in &mut model.protocols {
                let descriptor = self.settings.describe(
                    connection,
                    method,
                    &model.id,
                    protocol.api,
                    &snapshot,
                    &parameters,
                );
                protocol.settings = descriptor.settings.clone();
                protocol.warnings.extend(descriptor.warnings.clone());
                if descriptor.exact_catalog_match
                    && protocol.available
                    && exact_suggestion.is_none()
                {
                    exact_suggestion = Some(protocol.api);
                }
            }
            model.suggested_protocol =
                exact_suggestion.or(model.suggested_protocol).or_else(|| {
                    model
                        .protocols
                        .iter()
                        .find(|protocol| protocol.available)
                        .map(|protocol| protocol.api)
                });
        }
        for (name, source) in [
            ("models.dev", Source::Models),
            ("modelparams.dev", Source::Parameters),
        ] {
            result.source_status.insert(
                name.into(),
                serde_json::to_value(self.catalogs.status(source, Utc::now()))
                    .expect("source status serializes"),
            );
        }
        result
    }
}

pub fn inventory(list: ProviderModelList) -> Inventory {
    match list {
        ProviderModelList::CatalogFallback => Inventory::CatalogFallback,
        ProviderModelList::Listed { source, models } => {
            let mut seen = BTreeSet::new();
            let mut rows = Vec::new();
            for model in models.data {
                if model.id.is_empty()
                    || model.id.trim() != model.id
                    || model.id.len() > 1024
                    || model.id.chars().any(char::is_control)
                {
                    return Inventory::Error;
                }
                if seen.insert(model.id.clone()) {
                    rows.push(InventoryModel {
                        id: model.id,
                        name: model.name,
                        description: model.description,
                        context_window: model.context_length.map(u64::from),
                        max_tokens: model.max_output_tokens.map(u64::from),
                    });
                }
            }
            match source {
                ProviderModelListSource::Account => Inventory::Account(rows),
                ProviderModelListSource::Recipe => Inventory::Recipe(rows),
            }
        }
    }
}

pub fn configured_models(
    handle: &Handle,
    groups: &HashMap<String, ModelGroupConfig>,
) -> BTreeMap<String, Vec<String>> {
    fn visit(
        handle: &Handle,
        name: &str,
        model: &ModelGroupConfig,
        result: &mut BTreeMap<String, Vec<String>>,
    ) {
        if &model.provider == handle {
            let groups = result.entry(model.common.model.clone()).or_default();
            if !groups.iter().any(|group| group == name) {
                groups.push(name.into());
            }
        }
        for fallback in &model.common.fallbacks {
            visit(handle, name, fallback, result);
        }
    }
    let mut result = BTreeMap::new();
    for (name, group) in groups {
        visit(handle, name, group, &mut result);
    }
    for groups in result.values_mut() {
        groups.sort();
    }
    result
}

pub fn retain_saved_protocols(
    listing: &mut ModelListing,
    groups: &HashMap<String, ModelGroupConfig>,
) {
    fn visit(listing: &mut ModelListing, group: &ModelGroupConfig) {
        if group.provider == listing.connection {
            if let Some(api) = group.api
                && let Some(model) = listing
                    .models
                    .iter_mut()
                    .find(|model| model.id == group.common.model)
                && !model.protocols.iter().any(|protocol| protocol.api == api)
            {
                model.protocols.push(ModelProtocolInfo {
                    api,
                    available: false,
                    settings: vec![],
                    warnings: vec![
                        "unsupported_protocol_for_adapter; saved settings are preserved".into(),
                    ],
                });
            }
            if let Some(model) = listing
                .models
                .iter_mut()
                .find(|model| model.id == group.common.model)
            {
                let api = group
                    .api
                    .or_else(|| model.protocols.first().map(|protocol| protocol.api));
                if let Some(protocol) = model
                    .protocols
                    .iter_mut()
                    .find(|protocol| Some(protocol.api) == api)
                {
                    crate::inference::directory::settings::configured_settings(protocol, group);
                }
            }
        }
        for fallback in &group.common.fallbacks {
            visit(listing, fallback);
        }
    }
    for group in groups.values() {
        visit(listing, group);
    }
}

pub fn normalize(
    connection: &ResolvedConnection,
    method: Option<CredentialMethod>,
    eligible: &[ApiSurface],
    configured: BTreeMap<String, Vec<String>>,
    manual: &[String],
    inventory: Inventory,
    snapshot: &ModelCatalogSnapshot,
) -> ModelListing {
    let mut rows: BTreeMap<String, (ModelAvailability, InventoryModel, Vec<String>)> = configured
        .keys()
        .map(|id| {
            (
                id.clone(),
                (
                    ModelAvailability::Configured,
                    InventoryModel::default(),
                    vec!["configured".into()],
                ),
            )
        })
        .collect();
    let (source, mut status) = match &inventory {
        Inventory::Account(_) => ("account", "live"),
        Inventory::Recipe(_) => ("recipe", "recipe"),
        Inventory::CatalogFallback => ("catalog_fallback", "catalog_fallback"),
        Inventory::Error => ("live_error", "live_error"),
    };
    let discovered: Vec<(InventoryModel, ModelAvailability, &str)> = match inventory {
        Inventory::Account(models) => models
            .into_iter()
            .map(|model| (model, ModelAvailability::Account, "live"))
            .collect(),
        Inventory::Recipe(models) => models
            .into_iter()
            .map(|model| (model, ModelAvailability::Recipe, "recipe"))
            .collect(),
        Inventory::CatalogFallback => snapshot
            .entries
            .iter()
            .filter_map(|(key, model)| {
                if !connection.supports_catalog_route(model) {
                    return None;
                }
                key.strip_prefix(&format!("{}/", connection.brand))
                    .map(|id| {
                        (
                            InventoryModel {
                                id: id.into(),
                                name: model.name.clone(),
                                ..Default::default()
                            },
                            ModelAvailability::Catalog,
                            "models.dev",
                        )
                    })
            })
            .collect(),
        Inventory::Error => Vec::new(),
    };
    if status == "catalog_fallback" && discovered.is_empty() {
        status = if configured.is_empty() {
            "unavailable"
        } else {
            "configured_only"
        };
    }
    for (model, availability, provenance) in discovered {
        let row = rows.entry(model.id.clone()).or_insert((
            availability.clone(),
            InventoryModel::default(),
            vec![],
        ));
        row.0 = availability;
        row.1 = model;
        row.2.push(provenance.into());
    }
    for id in manual {
        let row = rows.entry(id.clone()).or_insert((
            ModelAvailability::Unverified,
            InventoryModel::default(),
            vec![],
        ));
        row.2.push("manual".into());
    }
    let models = rows
        .into_iter()
        .map(|(id, (availability, live, mut sources))| {
            let metadata = snapshot
                .entries
                .get(&format!("{}/{}", connection.brand, id));
            if metadata.is_some() && !sources.iter().any(|source| source == "models.dev") {
                sources.push("models.dev".into());
            }
            let mut warnings = Vec::new();
            if matches!(
                availability,
                ModelAvailability::Configured
                    | ModelAvailability::Unverified
                    | ModelAvailability::Catalog
                    | ModelAvailability::Recipe
            ) {
                warnings.push("Account access is unverified".into());
            }
            ProviderModelInfo {
                configured_in: configured.get(&id).cloned().unwrap_or_default(),
                context_window: live.context_window.filter(|value| *value > 0).or_else(|| {
                    metadata
                        .map(|model| model.limit.context)
                        .filter(|value| *value > 0)
                }),
                max_tokens: live
                    .max_tokens
                    .filter(|value| *value > 0)
                    .or_else(|| metadata.and_then(ModelEntry::max_output_tokens)),
                suggested_protocol: crate::inference::directory::protocols::protocol_default(
                    snapshot,
                    &connection.brand,
                    &id,
                )
                .map(ApiSurface::from)
                .filter(|protocol| connection.supports_model_protocol(&id, *protocol)),
                protocols: connection
                    .protocols
                    .iter()
                    .map(|api| ModelProtocolInfo {
                        api: *api,
                        available: eligible.contains(api)
                            && connection.supports_model_protocol(&id, *api),
                        settings: vec![],
                        warnings: if !connection.supports_model_protocol(&id, *api) {
                            vec!["unsupported_protocol_for_model_route".into()]
                        } else if eligible.contains(api) {
                            vec![]
                        } else {
                            vec!["unsupported_protocol_for_auth_method".into()]
                        },
                    })
                    .collect(),
                name: live
                    .name
                    .or_else(|| metadata.and_then(|model| model.name.clone())),
                description: live.description.or_else(|| {
                    metadata
                        .and_then(|model| model.authoring.get("description"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }),
                capabilities: metadata
                    .map(|model| ModelCapabilities {
                        reasoning: Some(model.reasoning),
                        tool_call: Some(model.tool_call),
                        structured_output: Some(model.structured_output),
                        input: model.modalities.input.clone(),
                        output: model.modalities.output.clone(),
                    })
                    .unwrap_or_default(),
                id,
                availability,
                sources,
                warnings,
            }
        })
        .collect();
    ModelListing {connection:connection.handle.clone(),credential_method:method,access_mode:connection.access_mode(method),source,directory_status:status,source_status:[("configured".into(),serde_json::json!({"status":"ok"})),("live".into(),serde_json::json!({"status":match source {"account"=>"ok","recipe"=>"recipe","catalog_fallback"=>"unsupported",_=>"error"}}))].into(),manual_entry:true,models}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ModelProviderConfig;
    use crate::inference::provider::platform::ProviderPlatform;
    use crate::{
        chat::broadcast::BroadcastService,
        inference::{
            credential::{runtime::RuntimeCredentials, store::ProviderCredentials},
            provider::{
                InferenceCounter,
                validation::{
                    CandidateCredential, ProviderValidationService, ValidationCandidate, binding,
                },
            },
        },
    };
    use frona_model_catalog::catalog::Limit;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn counter() -> InferenceCounter {
        InferenceCounter::new(BroadcastService::new())
    }

    async fn store() -> ProviderCredentials {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        crate::credential::managed::test_support::credentials(db, "directory-test-secret").await
    }

    async fn models(server: &MockServer, key: &str, id: &str) {
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", format!("Bearer {key}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object":"list", "data":[{"id":id,"object":"model","created":0,"owned_by":"test"}]
            })))
            .mount(server)
            .await;
    }

    fn account(endpoint: String, key: Option<&str>) -> ModelProviderConfig {
        ModelProviderConfig {
            provider: Some("openai".into()),
            base_url: Some(endpoint),
            api_key: key.map(String::from),
            ..Default::default()
        }
    }

    fn account_ids(inventory: Inventory) -> Vec<String> {
        match inventory {
            Inventory::Account(rows) => rows.into_iter().map(|row| row.id).collect(),
            other => panic!("expected account inventory, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn real_http_listing_distinguishes_empty_auth_transport_and_malformed_results() {
        let temp = tempfile::tempdir().unwrap();
        let mut directory = ModelDirectoryService::new(CatalogSources::load(temp.path()));
        directory.timeout = Duration::from_millis(100);
        for response in [
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object":"list","data":[]})),
            ResponseTemplate::new(401),
            ResponseTemplate::new(200).set_body_string("invalid json"),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[{"id":" "}]})),
            ResponseTemplate::new(200).set_delay(Duration::from_secs(1)),
        ]
        .into_iter()
        .enumerate()
        {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .respond_with(response.1)
                .mount(&server)
                .await;
            let config = account(server.uri(), Some("fixture"));
            let connection =
                ProviderPlatform::resolve(&Handle::const_validated("account"), &config).unwrap();
            let provider = crate::inference::provider::platform::build_provider(
                &connection,
                &config,
                &counter(),
            )
            .unwrap();
            let inventory = directory.draft_inventory(provider.as_ref()).await;
            if response.0 == 0 {
                assert!(account_ids(inventory).is_empty());
            } else {
                assert!(matches!(inventory, Inventory::Error));
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let config = account(endpoint, Some("fixture"));
        let connection =
            ProviderPlatform::resolve(&Handle::const_validated("account"), &config).unwrap();
        let provider =
            crate::inference::provider::platform::build_provider(&connection, &config, &counter())
                .unwrap();
        assert!(matches!(
            directory.draft_inventory(provider.as_ref()).await,
            Inventory::Error
        ));
    }

    #[tokio::test]
    async fn credential_generation_invalidates_only_its_account_and_empty_lists_are_cached() {
        let temp = tempfile::tempdir().unwrap();
        let directory = ModelDirectoryService::new(CatalogSources::load(temp.path()));
        let server = MockServer::start().await;
        for key in ["a", "b", "new-a"] {
            models(&server, key, key).await;
        }
        let store = store().await;
        let a = Handle::const_validated("account-a");
        let b = Handle::const_validated("account-b");
        let saved = store
            .vault()
            .create(
                "static",
                serde_json::json!({}),
                crate::credential::managed::integration::static_secret::document("a".into()),
            )
            .await
            .unwrap();
        let mut config_a = account(server.uri(), None);
        config_a.credential_id = Some(saved.item_id);
        let resolver = RuntimeCredentials::new(
            [
                (a.clone(), config_a.clone()),
                (b.clone(), account(server.uri(), Some("b"))),
            ]
            .into(),
            store.clone(),
            counter(),
        );
        for _ in 0..2 {
            for handle in [&a, &b] {
                let selected = resolver.resolve_for_listing(handle).await.unwrap();
                assert_eq!(
                    account_ids(directory.active_inventory(handle, &selected).await),
                    [if handle == &a { "a" } else { "b" }]
                );
            }
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        // Selecting either returned protocol and refreshing authoring metadata
        // must not refetch an otherwise fresh account list.
        let connection = ProviderPlatform::resolve(&a, &config_a).unwrap();
        for version in ["catalog-v1", "catalog-v2"] {
            let mut parameters = frona_model_catalog::parameters::ParameterCatalogSnapshot::empty();
            parameters.version = version.into();
            directory.catalogs.parameters.store(Arc::new(parameters));
            let selected = resolver.resolve_for_listing(&a).await.unwrap();
            let result = directory.listing(
                &connection,
                Some(selected.method),
                &selected.protocols,
                BTreeMap::new(),
                &[],
                directory.active_inventory(&a, &selected).await,
            );
            assert_eq!(result.models[0].protocols.len(), 2);
            assert!(
                result.models[0]
                    .protocols
                    .iter()
                    .all(|protocol| !protocol.settings.is_empty())
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
        let connection = ProviderPlatform::resolve(&a, &config_a).unwrap();
        let pending = store
            .stage(
                &a,
                CredentialMethod::ApiKey,
                binding(&connection, "database", &config_a),
                &Candidate::Secret(
                    crate::credential::managed::integration::static_secret::document(
                        "new-a".into(),
                    ),
                ),
            )
            .await
            .unwrap();
        store.promote(pending.validation_id, Some(1)).await.unwrap();
        let selected = resolver.resolve_for_listing(&a).await.unwrap();
        assert_eq!(
            account_ids(directory.active_inventory(&a, &selected).await),
            ["new-a"]
        );
        let selected_b = resolver.resolve_for_listing(&b).await.unwrap();
        assert_eq!(
            account_ids(directory.active_inventory(&b, &selected_b).await),
            ["b"]
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        let slot = directory.slots.lock().await.get(&a).unwrap().clone();
        slot.lock().await.as_mut().unwrap().expires = Instant::now();
        directory.active_inventory(&a, &selected).await;
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"object":"list","data":[]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        slot.lock().await.as_mut().unwrap().expires = Instant::now();
        for _ in 0..2 {
            assert!(account_ids(directory.active_inventory(&a, &selected).await).is_empty());
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn competing_validated_drafts_use_exact_credentials_without_populating_active_cache() {
        let temp = tempfile::tempdir().unwrap();
        let directory = ModelDirectoryService::new(CatalogSources::load(temp.path()));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&server)
            .await;
        for key in ["draft-a", "draft-b"] {
            models(&server, key, key).await;
        }
        let store = store().await;
        let validator = ProviderValidationService::new(store.clone(), counter());
        let handle = Handle::const_validated("account");
        let config = account(server.uri(), None);
        let mut proofs = Vec::new();
        for key in ["draft-a", "draft-b"] {
            proofs.push(
                validator
                    .validate(ValidationCandidate {
                        handle: handle.clone(),
                        config: config.clone(),
                        credential: CandidateCredential::New {
                            method: crate::inference::credential::store::CredentialMethod::ApiKey,
                            document:
                                crate::credential::managed::integration::static_secret::document(
                                    key.into(),
                                ),
                        },
                    })
                    .await
                    .unwrap(),
            );
        }
        let before = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            for (proof, key) in proofs.iter().zip(["draft-a", "draft-b"]) {
                let provider = validator
                    .draft_provider(
                        proof.pending.validation_id,
                        &handle,
                        &config,
                        CredentialMethod::ApiKey,
                        "database",
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    account_ids(directory.draft_inventory(provider.as_ref()).await),
                    [key]
                );
            }
        }
        assert_eq!(server.received_requests().await.unwrap().len(), before + 4);
        assert!(directory.slots.lock().await.is_empty());
        assert!(store.vault().list().await.unwrap().is_empty());
    }

    fn connection() -> ResolvedConnection {
        ProviderPlatform::resolve(
            &Handle::const_validated("account"),
            &ModelProviderConfig {
                provider: Some("openai".into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn snapshot() -> ModelCatalogSnapshot {
        let mut snapshot = ModelCatalogSnapshot::empty();
        for id in ["live", "catalog-only", "saved", "prefix"] {
            snapshot.entries.insert(
                format!("openai/{id}"),
                ModelEntry {
                    name: Some(format!("Name {id}")),
                    limit: Limit {
                        context: 1000,
                        output: 100,
                        input: None,
                    },
                    ..Default::default()
                },
            );
        }
        snapshot
    }

    fn rows(result: &ModelListing) -> Vec<&str> {
        result
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect()
    }

    #[test]
    fn authoritative_empty_and_nonempty_lists_never_append_catalog_rows() {
        for live in [
            vec![],
            vec![InventoryModel {
                id: "live".into(),
                name: None,
                ..Default::default()
            }],
        ] {
            let expected = live.len() + 2;
            let result = normalize(
                &connection(),
                Some(CredentialMethod::ApiKey),
                &[ApiSurface::Completions],
                [("saved".into(), vec!["primary".into()])].into(),
                &["manual".into()],
                Inventory::Account(live),
                &snapshot(),
            );
            assert_eq!(result.directory_status, "live");
            assert_eq!(result.models.len(), expected);
            assert!(!rows(&result).contains(&"catalog-only"));
            assert!(result.manual_entry);
            let saved = result
                .models
                .iter()
                .find(|model| model.id == "saved")
                .unwrap();
            assert!(matches!(saved.availability, ModelAvailability::Configured));
            assert_eq!(saved.context_window, Some(1000));
            assert_eq!(saved.max_tokens, Some(100));
            let manual = result
                .models
                .iter()
                .find(|model| model.id == "manual")
                .unwrap();
            assert!(matches!(manual.availability, ModelAvailability::Unverified));
        }
    }

    #[test]
    fn fallback_recipe_errors_and_absent_catalogs_have_distinct_provenance() {
        let connection = connection();
        let snapshot = snapshot();
        for (inventory, status, extra) in [
            (Inventory::CatalogFallback, "catalog_fallback", true),
            (Inventory::Error, "live_error", false),
            (
                Inventory::Recipe(vec![InventoryModel {
                    id: "recipe".into(),
                    name: None,
                    ..Default::default()
                }]),
                "recipe",
                false,
            ),
        ] {
            let result = normalize(
                &connection,
                None,
                &[],
                [("saved".into(), vec!["primary".into()])].into(),
                &["manual".into()],
                inventory,
                &snapshot,
            );
            assert_eq!(result.directory_status, status);
            assert_eq!(rows(&result).contains(&"catalog-only"), extra);
            assert!(rows(&result).contains(&"saved"));
            assert!(rows(&result).contains(&"manual"));
            if let Some(recipe) = result.models.iter().find(|model| model.id == "recipe") {
                assert!(matches!(recipe.availability, ModelAvailability::Recipe));
            }
        }
        let missing = normalize(
            &connection,
            None,
            &[],
            BTreeMap::new(),
            &[],
            Inventory::CatalogFallback,
            &ModelCatalogSnapshot::empty(),
        );
        assert_eq!(missing.directory_status, "unavailable");
        assert!(missing.manual_entry);
        let missing = normalize(
            &connection,
            None,
            &[],
            [("saved".into(), vec![])].into(),
            &[],
            Inventory::CatalogFallback,
            &ModelCatalogSnapshot::empty(),
        );
        assert_eq!(missing.directory_status, "configured_only");
    }

    #[test]
    fn primary_fallbacks_and_manual_ids_keep_exact_identity_without_prefix_enrichment() {
        let groups=[("primary".into(),serde_json::from_value(serde_json::json!({"provider":"account","model":"saved","fallbacks":[{"provider":"account","model":"fallback"},{"provider":"other","model":"other"}]})).unwrap())].into();
        let configured = configured_models(&Handle::const_validated("account"), &groups);
        assert_eq!(configured.len(), 2);
        let result = normalize(
            &connection(),
            Some(CredentialMethod::ApiKey),
            &[ApiSurface::Completions],
            configured,
            &["prefix-2026".into()],
            Inventory::Account(vec![]),
            &snapshot(),
        );
        let manual = result
            .models
            .iter()
            .find(|model| model.id == "prefix-2026")
            .unwrap();
        assert!(manual.name.is_none());
        assert!(manual.context_window.is_none());
        assert!(manual.max_tokens.is_none());
        assert_eq!(
            result
                .models
                .iter()
                .find(|model| model.id == "fallback")
                .unwrap()
                .configured_in,
            ["primary"]
        );
    }

    #[test]
    fn saved_ineligible_and_unsupported_protocols_remain_visible_without_rewriting_groups() {
        let groups = [("primary".into(), serde_json::from_value(serde_json::json!({
            "provider":"account", "model":"saved", "api":"anthropic-messages",
            "temperature":0.7, "fallbacks":[{"provider":"account","model":"saved","api":"completions"}]
        })).unwrap())].into();
        let before = serde_json::to_value(&groups).unwrap();
        let mut result = normalize(
            &connection(),
            Some(CredentialMethod::Oauth),
            &[ApiSurface::Responses],
            configured_models(&Handle::const_validated("account"), &groups),
            &[],
            Inventory::Account(vec![]),
            &snapshot(),
        );
        retain_saved_protocols(&mut result, &groups);
        let protocols = &result.models[0].protocols;
        assert!(
            protocols
                .iter()
                .find(|protocol| protocol.api == ApiSurface::Responses)
                .unwrap()
                .available
        );
        for api in [ApiSurface::Completions, ApiSurface::AnthropicMessages] {
            let protocol = protocols
                .iter()
                .find(|protocol| protocol.api == api)
                .unwrap();
            assert!(!protocol.available);
            assert!(!protocol.warnings.is_empty());
        }
        assert_eq!(serde_json::to_value(&groups).unwrap(), before);
    }
}
