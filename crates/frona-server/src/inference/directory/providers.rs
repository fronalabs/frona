//! Authoring-only provider identity and setup descriptors. Catalog hints never
//! register executable authentication or participate in runtime routing.

use crate::inference::provider::{
    platform::{ProviderPlatform, ResolvedConnection},
    service::ProviderInspection,
};
use crate::{
    core::{
        Handle,
        config::{AdapterId, ApiSurface, ModelProviderConfig},
    },
    inference::credential::store::CredentialMethod,
};
use chrono::{DateTime, Utc};
use frona_model_catalog::{
    ModelCatalogSnapshot,
    catalog::ProviderEntry,
    sources::{CatalogSources, Source, SourceStatus},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderFieldTarget {
    Configuration { path: String },
    Credential { field: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderFieldInfo {
    pub id: String,
    pub target: ProviderFieldTarget,
    pub label: String,
    pub required: bool,
    pub sensitive: bool,
    pub schema: Value,
    pub suggested_env: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthMethodInfo {
    pub id: String,
    pub credential_method: CredentialMethod,
    pub access_mode: &'static str,
    pub priority: u16,
    pub protocols: Vec<ApiSurface>,
    pub interaction: &'static str,
    pub persistence: &'static str,
    pub fields: Vec<ProviderFieldInfo>,
    pub validation_available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub documentation_url: Option<String>,
    pub logo_url: Option<String>,
    pub adapter: AdapterId,
    pub default_base_url: Option<String>,
    pub api_surfaces: Vec<ApiSurface>,
    pub auth_methods: Vec<AuthMethodInfo>,
    pub fields: Vec<ProviderFieldInfo>,
    pub configuration_defaults: Value,
    pub sources: Vec<&'static str>,
    pub warnings: Vec<String>,
    pub catalog_available: bool,
    pub effective_protocols: Option<Vec<ApiSurface>>,
}

#[derive(Serialize)]
pub struct ProviderCatalog {
    pub providers: Vec<ProviderCatalogEntry>,
    pub source_status: HashMap<&'static str, SourceStatus>,
}

pub fn catalog(sources: &CatalogSources, now: DateTime<Utc>) -> ProviderCatalog {
    ProviderCatalog {
        providers: available(&sources.models.current()),
        source_status: [
            ("models.dev", sources.status(Source::Models, now)),
            ("modelparams.dev", sources.status(Source::Parameters, now)),
        ]
        .into(),
    }
}

pub fn available(snapshot: &ModelCatalogSnapshot) -> Vec<ProviderCatalogEntry> {
    let brands: BTreeSet<&str> = snapshot
        .providers
        .keys()
        .map(String::as_str)
        .chain(ProviderPlatform::built_in_brands().iter().copied())
        .collect();
    brands
        .into_iter()
        .filter_map(|brand| {
            let record = snapshot.providers.get(brand);
            let built_in = ProviderPlatform::has_recipe(brand);
            // The generic API-key contract is compiled, not inferred from arbitrary
            // environment names or a catalog-supplied OAuth/cloud configuration.
            if !built_in
                && record.is_some_and(|record| {
                    record.authoring.contains_key("auth")
                        || (!record.env.is_empty() && api_key_hints(record).is_empty())
                })
            {
                return None;
            }
            let adapter = if built_in {
                None
            } else {
                Some(ProviderPlatform::adapter_for_npm(record?.npm.as_deref()?)?)
            };
            let connection = ModelProviderConfig {
                provider: Some(brand.into()),
                adapter,
                base_url: if built_in { None } else { record?.api.clone() },
                ..Default::default()
            };
            let handle = Handle::const_validated("catalog-draft");
            let resolved = ProviderPlatform::resolve(&handle, &connection).ok()?;
            // A per-model route using a different protocol/host is not silently
            // converted into a generic provider-wide recipe.
            if !built_in
                && snapshot.entries.iter().any(|(key, model)| {
                    key.starts_with(&format!("{brand}/"))
                        && model.provider.as_ref().is_some_and(|route| {
                            route.npm.as_deref().is_some_and(|npm| {
                                ProviderPlatform::adapter_for_npm(npm) != Some(resolved.adapter)
                            }) || route
                                .api
                                .as_deref()
                                .is_some_and(|api| Some(api) != connection.base_url.as_deref())
                        })
                })
            {
                return None;
            }
            let entry = describe(&resolved, &connection, record, false);
            (!entry.auth_methods.is_empty()).then_some(entry)
        })
        .collect()
}

fn api_key_hints(record: &ProviderEntry) -> Vec<String> {
    record
        .env
        .iter()
        .filter(|name| {
            name.ends_with("API_KEY")
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
        .cloned()
        .collect()
}

fn safe_link(value: Option<&str>) -> Option<String> {
    let url = reqwest::Url::parse(value?).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

fn describe(
    resolved: &ResolvedConnection,
    config: &ModelProviderConfig,
    record: Option<&ProviderEntry>,
    configured: bool,
) -> ProviderCatalogEntry {
    let built_in = ProviderPlatform::has_recipe(&resolved.brand);
    let mut defaults = json!({"provider":resolved.brand});
    if !built_in {
        defaults["adapter"] = json!(resolved.adapter);
        defaults["base_url"] = json!(config.base_url);
    }
    let endpoint_required = (!built_in && resolved.adapter != AdapterId::Ollama)
        || resolved.factory == crate::inference::provider::platform::FactoryKind::Azure;
    let mut fields = vec![
        ProviderFieldInfo {
            id: "base_url".into(),
            target: ProviderFieldTarget::Configuration {
                path: "/base_url".into(),
            },
            label: "Endpoint".into(),
            required: endpoint_required,
            sensitive: false,
            schema: json!({"type":"string","format":"uri"}),
            suggested_env: vec![],
        },
        ProviderFieldInfo {
            id: "enabled".into(),
            target: ProviderFieldTarget::Configuration {
                path: "/enabled".into(),
            },
            label: "Enabled".into(),
            required: false,
            sensitive: false,
            schema: json!({"type":"boolean","default":true}),
            suggested_env: vec![],
        },
    ];
    if resolved.factory == crate::inference::provider::platform::FactoryKind::Azure {
        fields.push(ProviderFieldInfo {
            id: "azure_api_version".into(),
            target: ProviderFieldTarget::Configuration { path: "/azure_api_version".into() },
            label: "Azure API version".into(),
            required: false,
            sensitive: false,
            schema: json!({"type":"string","pattern":"^[A-Za-z0-9-]+$","default":crate::inference::provider::adapter::azure::DEFAULT_API_VERSION}),
            suggested_env: vec![],
        });
    }
    if resolved.adapter == AdapterId::Bedrock {
        for (id, label, env) in [
            ("aws_region", "AWS region", "AWS_REGION"),
            ("aws_profile", "AWS profile", "AWS_PROFILE"),
        ] {
            fields.push(ProviderFieldInfo {
                id: id.into(),
                target: ProviderFieldTarget::Configuration {
                    path: format!("/{id}"),
                },
                label: label.into(),
                required: false,
                sensitive: false,
                schema: json!({"type":"string","minLength":1}),
                suggested_env: vec![env.into()],
            });
        }
    }
    let mut methods: Vec<AuthMethodInfo> = resolved
        .auth_methods
        .iter()
        .filter(|method| configured || ProviderPlatform::has_validator(resolved, method.method))
        .map(|method| {
            let key = method.method == CredentialMethod::ApiKey;
            AuthMethodInfo {
                id: method.id.into(),
                credential_method: method.method,
                access_mode: resolved.access_mode(Some(method.method)),
                priority: method.priority,
                protocols: method.protocols.to_vec(),
                interaction: if method.method == CredentialMethod::Oauth {
                    "backend_login"
                } else if key {
                    "form"
                } else if method.method == CredentialMethod::Aws {
                    "ambient_credentials"
                } else {
                    "none"
                },
                persistence: if key || method.method == CredentialMethod::Oauth {
                    "database"
                } else {
                    "none"
                },
                validation_available: ProviderPlatform::has_validator(resolved, method.method),
                fields: if key {
                    vec![ProviderFieldInfo {
                        id: "api_key".into(),
                        target: ProviderFieldTarget::Credential {
                            field: "api_key".into(),
                        },
                        label: if resolved.factory
                            == crate::inference::provider::platform::FactoryKind::Copilot
                        {
                            "GitHub access token".into()
                        } else {
                            "API key".into()
                        },
                        required: true,
                        sensitive: true,
                        schema: json!({"type":"string","minLength":1}),
                        suggested_env: if resolved.factory
                            == crate::inference::provider::platform::FactoryKind::Copilot
                        {
                            vec!["GITHUB_COPILOT_TOKEN".into(), "GITHUB_TOKEN".into()]
                        } else if resolved.adapter == AdapterId::Bedrock {
                            vec!["AWS_BEARER_TOKEN_BEDROCK".into()]
                        } else {
                            record.map(api_key_hints).unwrap_or_default()
                        },
                    }]
                } else {
                    vec![]
                },
            }
        })
        .collect();
    if crate::inference::provider::platform::accepts_openrouter(resolved) {
        methods.push(AuthMethodInfo {
            id: "openrouter_connect".into(),
            credential_method: CredentialMethod::ApiKey,
            access_mode: "api",
            priority: 20,
            protocols: resolved.protocols.to_vec(),
            interaction: "backend_login",
            persistence: "database",
            fields: vec![],
            validation_available: true,
        });
    }
    methods.sort_by_key(|method| method.priority);
    let mut warnings = Vec::new();
    if configured && methods.iter().all(|method| !method.validation_available) {
        warnings.push("No implemented live validator; existing connection is editable but is not offered for new setup".into());
    }
    let mut sources = Vec::new();
    if record.is_some() {
        sources.push("models.dev");
    }
    if built_in {
        sources.push("recipe");
    }
    if configured {
        sources.push("configured");
    }
    ProviderCatalogEntry {
        id: resolved.brand.clone(),
        name: record
            .filter(|record| !record.name.is_empty())
            .map(|record| record.name.clone())
            .unwrap_or_else(|| built_in_name(&resolved.brand)),
        description: record
            .and_then(|record| record.authoring.get("description"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        documentation_url: safe_link(record.and_then(|record| record.doc.as_deref())),
        logo_url: record
            .and_then(|record| safe_link(record.authoring.get("logo").and_then(Value::as_str)))
            .or_else(|| record.map(|_| format!("https://models.dev/logos/{}.svg", resolved.brand))),
        adapter: resolved.adapter,
        default_base_url: resolved.effective_base_url.clone(),
        api_surfaces: resolved.protocols.to_vec(),
        auth_methods: methods,
        fields,
        configuration_defaults: defaults,
        sources,
        warnings,
        catalog_available: record.is_some(),
        effective_protocols: None,
    }
}

fn built_in_name(brand: &str) -> String {
    match brand {
        "github-copilot" => "GitHub Copilot",
        "azure" => "Azure OpenAI",
        "openai" => "OpenAI",
        "anthropic" => "Anthropic",
        "kimi-for-coding" => "Kimi Code",
        "google" => "Google",
        "ollama" => "Ollama",
        "xai" => "xAI",
        "openrouter" => "OpenRouter",
        "deepseek" => "DeepSeek",
        "moonshotai" => "Moonshot AI",
        "togetherai" => "Together AI",
        "huggingface" => "Hugging Face",
        other => other,
    }
    .into()
}

pub fn decorate(inspection: &mut ProviderInspection, snapshot: &ModelCatalogSnapshot) {
    let mut value = inspection.configuration.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("api_key");
    }
    let Ok(mut config) = serde_json::from_value::<ModelProviderConfig>(value) else {
        return;
    };
    config.provider = Some(inspection.provider.clone());
    config.adapter = Some(inspection.adapter);
    let Ok(resolved) = ProviderPlatform::resolve(&inspection.handle, &config) else {
        return;
    };
    let mut entry = describe(
        &resolved,
        &config,
        snapshot.providers.get(&inspection.provider),
        true,
    );
    if let Some(effective) = &inspection.effective_authentication {
        entry.effective_protocols = Some(
            entry
                .auth_methods
                .iter()
                .filter(|method| Some(method.credential_method) == effective.method)
                .flat_map(|method| method.protocols.iter().copied())
                .fold(Vec::new(), |mut protocols, protocol| {
                    if !protocols.contains(&protocol) {
                        protocols.push(protocol);
                    }
                    protocols
                }),
        );
    }
    inspection.setup = Some(entry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use frona_model_catalog::catalog::{ModelEntry, ModelRoute};

    #[test]
    fn openrouter_connect_is_an_api_key_interaction_only_on_the_trusted_endpoint() {
        for endpoint in [None, Some("https://custom.invalid/v1".to_owned())] {
            let config = ModelProviderConfig {
                provider: Some("openrouter".into()),
                base_url: endpoint.clone(),
                ..Default::default()
            };
            let resolved =
                ProviderPlatform::resolve(&Handle::const_validated("router"), &config).unwrap();
            let entry = describe(&resolved, &config, None, true);
            let connect = entry
                .auth_methods
                .iter()
                .find(|method| method.id == "openrouter_connect");
            assert_eq!(connect.is_some(), endpoint.is_none());
            if let Some(connect) = connect {
                assert_eq!(connect.credential_method, CredentialMethod::ApiKey);
                assert_eq!(connect.interaction, "backend_login");
                assert_eq!(connect.persistence, "database");
            }
            assert!(
                entry
                    .auth_methods
                    .iter()
                    .any(|method| method.interaction == "form")
            );
        }
    }

    fn record(npm: &str, endpoint: Option<&str>, env: &[&str]) -> ProviderEntry {
        ProviderEntry {
            id: "new-brand".into(),
            name: "New Brand".into(),
            npm: Some(npm.into()),
            api: endpoint.map(str::to_owned),
            env: env.iter().map(|value| (*value).into()).collect(),
            doc: Some("https://docs.example/setup".into()),
            ..Default::default()
        }
    }

    #[test]
    fn dynamic_brands_use_compiled_contracts_and_flat_targets() {
        let mut snapshot = ModelCatalogSnapshot::empty();
        snapshot.providers.insert(
            "new-brand".into(),
            record(
                "@ai-sdk/openai-compatible",
                Some("https://api.example/v1"),
                &["NEW_API_KEY"],
            ),
        );
        let catalog = available(&snapshot);
        let brand = catalog
            .iter()
            .find(|brand| brand.id == "new-brand")
            .unwrap();
        assert_eq!(brand.name, "New Brand");
        assert_eq!(brand.adapter, AdapterId::Openai);
        assert_eq!(
            brand.configuration_defaults,
            json!({"provider":"new-brand","adapter":"openai","base_url":"https://api.example/v1"})
        );
        assert!(
            matches!(&brand.fields[0].target,ProviderFieldTarget::Configuration{path} if path=="/base_url")
        );
        let key = &brand.auth_methods[0].fields[0];
        assert!(matches!(&key.target,ProviderFieldTarget::Credential{field} if field=="api_key"));
        assert!(key.sensitive);
        assert_eq!(key.suggested_env, ["NEW_API_KEY"]);
        assert!(brand.auth_methods[0].validation_available);
        assert!(brand.api_surfaces.contains(&ApiSurface::Completions));
        let defaults: ModelProviderConfig =
            serde_json::from_value(brand.configuration_defaults.clone()).unwrap();
        assert!(
            ProviderPlatform::resolve(&Handle::const_validated("new-account"), &defaults).is_ok()
        );
    }

    #[test]
    fn missing_unknown_or_conflicting_contracts_are_omitted_not_disabled() {
        for fixture in [
            record(
                "@unknown/sdk",
                Some("https://api.example"),
                &["NEW_API_KEY"],
            ),
            record("@ai-sdk/openai-compatible", None, &["NEW_API_KEY"]),
            record(
                "@ai-sdk/openai-compatible",
                Some("https://api.example"),
                &["AWS_PROFILE", "AWS_SECRET_ACCESS_KEY"],
            ),
            record(
                "@ai-sdk/openai-compatible",
                Some("https://user:secret@api.example"),
                &["NEW_API_KEY"],
            ),
        ] {
            let mut snapshot = ModelCatalogSnapshot::empty();
            snapshot.providers.insert("new-brand".into(), fixture);
            assert!(
                !available(&snapshot)
                    .iter()
                    .any(|brand| brand.id == "new-brand")
            );
        }
        for (npm, api) in [
            (Some("@ai-sdk/anthropic"), None),
            (None, Some("https://other.example")),
        ] {
            let mut snapshot = ModelCatalogSnapshot::empty();
            snapshot.providers.insert(
                "new-brand".into(),
                record(
                    "@ai-sdk/openai-compatible",
                    Some("https://api.example"),
                    &["NEW_API_KEY"],
                ),
            );
            snapshot.entries.insert(
                "new-brand/model".into(),
                ModelEntry {
                    provider: Some(ModelRoute {
                        npm: npm.map(str::to_owned),
                        api: api.map(str::to_owned),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
            assert!(
                !available(&snapshot)
                    .iter()
                    .any(|brand| brand.id == "new-brand")
            );
        }
    }

    #[test]
    fn absent_builtins_are_added_but_unimplemented_validators_are_hidden() {
        let brands = available(&ModelCatalogSnapshot::empty());
        assert!(
            brands
                .iter()
                .any(|brand| brand.id == "openai" && brand.name == "OpenAI")
        );
        assert!(
            brands
                .iter()
                .any(|brand| brand.id == "ollama" && brand.auth_methods[0].interaction == "none")
        );
        assert!(!brands.iter().any(|brand| brand.id == "huggingface"));
        let bedrock = brands
            .iter()
            .find(|brand| brand.id == "amazon-bedrock")
            .unwrap();
        assert_eq!(
            bedrock
                .auth_methods
                .iter()
                .map(|method| method.credential_method)
                .collect::<Vec<_>>(),
            vec![CredentialMethod::ApiKey, CredentialMethod::Aws]
        );
        assert_eq!(bedrock.auth_methods[1].interaction, "ambient_credentials");
        assert!(bedrock.fields.iter().any(|field| matches!(&field.target, ProviderFieldTarget::Configuration { path } if path == "/aws_region")));
        assert!(bedrock.fields.iter().any(|field| matches!(&field.target, ProviderFieldTarget::Configuration { path } if path == "/aws_profile")));
        for brand in brands {
            assert!(
                brand
                    .auth_methods
                    .iter()
                    .all(|method| method.validation_available)
            );
        }
    }

    #[test]
    fn configured_removed_brand_is_editable_without_catalog_routing() {
        let mut inspection = ProviderInspection {
            setup: None,
            handle: Handle::const_validated("existing"),
            provider: "removed-brand".into(),
            adapter: AdapterId::Openai,
            configuration: json!({"provider":"removed-brand","adapter":"openai","base_url":"https://saved.example/v1","api_key":{"is_set":true}}),
            pending_removal: false,
            effective_authentication: Some(
                crate::inference::credential::runtime::EffectiveAuthentication {
                    method: Some(CredentialMethod::ApiKey),
                    source: "database".into(),
                },
            ),
            authentication_methods: vec![],
            credentials: vec![],
            affected_groups: vec![],
        };
        decorate(&mut inspection, &ModelCatalogSnapshot::empty());
        let setup = inspection.setup.unwrap();
        assert!(!setup.catalog_available);
        assert_eq!(
            setup.default_base_url.as_deref(),
            Some("https://saved.example/v1")
        );
        assert_eq!(
            setup.effective_protocols,
            Some(vec![ApiSurface::Completions, ApiSurface::Responses])
        );
        assert!(setup.warnings.is_empty());
        assert!(setup.configuration_defaults.get("api_key").is_none());
        assert!(
            !available(&ModelCatalogSnapshot::empty())
                .iter()
                .any(|brand| brand.id == "removed-brand")
        );
    }

    #[test]
    fn missing_catalogs_report_unavailable_without_hiding_compiled_recipes() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CatalogSources::load(dir.path());
        let result = catalog(&sources, Utc::now() + chrono::Duration::days(2));
        assert_eq!(result.source_status["models.dev"].state, "unavailable");
        assert!(
            result
                .providers
                .iter()
                .any(|provider| provider.id == "openai")
        );
        assert!(result.source_status["modelparams.dev"].version.is_none());
    }

    #[tokio::test]
    async fn newly_cataloged_brand_validates_through_the_real_compiled_path() {
        use crate::chat::broadcast::BroadcastService;
        use crate::inference::provider::{
            InferenceCounter,
            validation::{CandidateCredential, ProviderValidationService, ValidationCandidate},
        };
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/models")).and(header("authorization","Bearer fixture-secret")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[{"id":"test-model","object":"model","created":0,"owned_by":"fixture"}]}))).with_priority(1).mount(&server).await;
        let mut snapshot = ModelCatalogSnapshot::empty();
        snapshot.providers.insert(
            "new-brand".into(),
            record(
                "@ai-sdk/openai-compatible",
                Some(&server.uri()),
                &["NEW_API_KEY"],
            ),
        );
        let descriptor = available(&snapshot)
            .into_iter()
            .find(|brand| brand.id == "new-brand")
            .unwrap();
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let store =
            crate::credential::managed::test_support::credentials(db, "fixture-encryption").await;
        let validator = ProviderValidationService::new(
            store.clone(),
            InferenceCounter::new(BroadcastService::new()),
        );
        let handle = Handle::const_validated("brand-account");
        let proof = validator
            .validate(ValidationCandidate {
                handle: handle.clone(),
                config: serde_json::from_value(descriptor.configuration_defaults).unwrap(),
                credential: CandidateCredential::New {
                    method: crate::inference::credential::store::CredentialMethod::ApiKey,
                    document: crate::credential::managed::integration::static_secret::document(
                        "fixture-secret".into(),
                    ),
                },
            })
            .await
            .unwrap();
        assert_eq!(proof.pending.binding.provider, "new-brand");
        assert_eq!(proof.pending.handle, handle);
        assert!(store.vault().list().await.unwrap().is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
