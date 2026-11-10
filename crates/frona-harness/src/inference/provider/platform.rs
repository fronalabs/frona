use std::sync::Arc;

use rig_core::http_client::DynHttpClient;
use rig_core::providers::{
    anthropic, ollama, openai,
    registry::{ProviderConfig, ProviderId},
};

use crate::Handle;
use crate::config::CredentialMethod;
use crate::config::{AdapterId, ApiSurface, ModelProviderConfig};

use crate::inference::error::InferenceError;
use crate::inference::provider::{
    CredentialValidationError, InferenceCounter, ProviderAdmin, ProviderConnection, ResolvedModel,
    RigProvider,
};

#[derive(Debug, Clone, Copy)]
pub struct AuthMethodDescriptor {
    pub id: &'static str,
    pub method: CredentialMethod,
    pub priority: u16,
    pub protocols: &'static [ApiSurface],
}

#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub handle: Handle,
    pub brand: String,
    pub adapter: AdapterId,
    /// Supported protocols, with the default request surface first.
    pub protocols: &'static [ApiSurface],
    pub auth_methods: &'static [AuthMethodDescriptor],
    pub effective_base_url: Option<String>,
    pub rig_provider: Option<ProviderId>,
}

impl ResolvedConnection {
    pub fn supports_model_protocol(&self, model: &str, api: ApiSurface) -> bool {
        self.protocols.contains(&api)
            && (self.brand != "github-copilot"
                || crate::inference::provider::adapter::copilot::protocol(model) == api)
    }

    pub fn supports_catalog_route(&self, model: &frona_model_catalog::catalog::ModelEntry) -> bool {
        self.brand != "azure"
            || model.provider.as_ref().is_none_or(|route| {
                route
                    .npm
                    .as_deref()
                    .is_none_or(|npm| npm == "@ai-sdk/azure")
                    && route.api.is_none()
            })
    }

    /// Catalog aliases and commercial access belong to compiled recipes, not
    /// credential-mechanism guesses or catalog routing.
    pub fn catalog_identity(&self, method: Option<CredentialMethod>) -> (&str, &'static str) {
        match self.brand.as_str() {
            "github-copilot" => ("github-copilot", "subscription"),
            "amazon-bedrock" => ("bedrock", "api_key"),
            "kimi-for-coding" => ("moonshot", "subscription"),
            "moonshotai" => ("moonshot", "api_key"),
            "openai" if method == Some(CredentialMethod::Oauth) => ("openai", "subscription"),
            brand => (brand, "api_key"),
        }
    }

    pub fn access_mode(&self, method: Option<CredentialMethod>) -> &'static str {
        if self.catalog_identity(method).1 == "subscription" {
            "subscription"
        } else {
            "api"
        }
    }

    pub fn request_adapter_name(&self) -> &'static str {
        if self.adapter == AdapterId::Gemini {
            return "gemini";
        }
        match self.brand.as_str() {
            "github-copilot" | "azure" => "openai",
            // These recipes intentionally use generic OpenAI wire behavior.
            "xai" => "xai",
            "galadriel" => "galadriel",
            _ => self
                .rig_provider
                .as_ref()
                .map_or("bedrock", ProviderId::vendor),
        }
    }
}

pub struct ProviderPlatform;

const COMPLETIONS: &[ApiSurface] = &[ApiSurface::Completions];
const OPENAI_PROTOCOLS: &[ApiSurface] = &[ApiSurface::Completions, ApiSurface::Responses];
const OPENAI_PREFERRED_PROTOCOLS: &[ApiSurface] = &[ApiSurface::Responses, ApiSurface::Completions];
const ANTHROPIC_PROTOCOLS: &[ApiSurface] = &[ApiSurface::AnthropicMessages];
const GEMINI_PROTOCOLS: &[ApiSurface] = &[ApiSurface::GoogleGenerateContent];
const COHERE_PROTOCOLS: &[ApiSurface] = &[ApiSurface::CohereChat];
const OLLAMA_PROTOCOLS: &[ApiSurface] = &[ApiSurface::Ollama];
const HUGGINGFACE_PROTOCOLS: &[ApiSurface] = &[ApiSurface::HuggingFace];
const BEDROCK_PROTOCOLS: &[ApiSurface] = &[ApiSurface::AmazonBedrockConverse];
const BEDROCK_AUTH: &[AuthMethodDescriptor] = &[
    AuthMethodDescriptor {
        id: "api_key",
        method: CredentialMethod::ApiKey,
        priority: 10,
        protocols: BEDROCK_PROTOCOLS,
    },
    AuthMethodDescriptor {
        id: "aws",
        method: CredentialMethod::Aws,
        priority: 20,
        protocols: BEDROCK_PROTOCOLS,
    },
];

const API_KEY_OPENAI: &[AuthMethodDescriptor] = &[AuthMethodDescriptor {
    id: "api_key",
    method: CredentialMethod::ApiKey,
    priority: 10,
    protocols: OPENAI_PROTOCOLS,
}];

const OPENAI_AUTH: &[AuthMethodDescriptor] = &[
    AuthMethodDescriptor {
        id: "api_key",
        method: CredentialMethod::ApiKey,
        priority: 10,
        protocols: OPENAI_PROTOCOLS,
    },
    AuthMethodDescriptor {
        id: "chatgpt_oauth",
        method: CredentialMethod::Oauth,
        priority: 20,
        protocols: &[ApiSurface::Responses],
    },
];

const COPILOT_AUTH: &[AuthMethodDescriptor] = &[
    AuthMethodDescriptor {
        id: "api_key",
        method: CredentialMethod::ApiKey,
        priority: 10,
        protocols: OPENAI_PROTOCOLS,
    },
    AuthMethodDescriptor {
        id: "oauth",
        method: CredentialMethod::Oauth,
        priority: 20,
        protocols: OPENAI_PROTOCOLS,
    },
];

const API_KEY_COMPLETIONS: &[AuthMethodDescriptor] = &[AuthMethodDescriptor {
    id: "api_key",
    method: CredentialMethod::ApiKey,
    priority: 10,
    protocols: COMPLETIONS,
}];

const API_KEY_ANTHROPIC: &[AuthMethodDescriptor] = &[AuthMethodDescriptor {
    id: "api_key",
    method: CredentialMethod::ApiKey,
    priority: 10,
    protocols: ANTHROPIC_PROTOCOLS,
}];

const API_KEY_GEMINI: &[AuthMethodDescriptor] = &[AuthMethodDescriptor {
    id: "api_key",
    method: CredentialMethod::ApiKey,
    priority: 10,
    protocols: GEMINI_PROTOCOLS,
}];

const API_KEY_COHERE: &[AuthMethodDescriptor] = &[AuthMethodDescriptor {
    id: "api_key",
    method: CredentialMethod::ApiKey,
    priority: 10,
    protocols: COHERE_PROTOCOLS,
}];

const API_KEY_HUGGINGFACE: &[AuthMethodDescriptor] = &[AuthMethodDescriptor {
    id: "api_key",
    method: CredentialMethod::ApiKey,
    priority: 10,
    protocols: HUGGINGFACE_PROTOCOLS,
}];

const ANONYMOUS_OLLAMA: &[AuthMethodDescriptor] = &[AuthMethodDescriptor {
    id: "anonymous",
    method: CredentialMethod::Anonymous,
    priority: 10,
    protocols: OLLAMA_PROTOCOLS,
}];

impl ProviderPlatform {
    pub fn built_in_brands() -> &'static [&'static str] {
        &[
            "github-copilot",
            "azure",
            "amazon-bedrock",
            "openai",
            "anthropic",
            "kimi-for-coding",
            "ollama",
            "groq",
            "openrouter",
            "deepseek",
            "google",
            "cohere",
            "mistral",
            "perplexity",
            "togetherai",
            "xai",
            "hyperbolic",
            "moonshotai",
            "mira",
            "galadriel",
            "huggingface",
        ]
    }

    pub fn has_recipe(brand: &str) -> bool {
        recipe(brand).is_some()
    }

    pub fn adapter_for_npm(npm: &str) -> Option<AdapterId> {
        match npm {
            "@ai-sdk/openai"
            | "@ai-sdk/openai-compatible"
            | "@ai-sdk/deepseek"
            | "@ai-sdk/groq"
            | "@ai-sdk/mistral"
            | "@ai-sdk/xai"
            | "@ai-sdk/togetherai"
            | "@ai-sdk/perplexity"
            | "@openrouter/ai-sdk-provider" => Some(AdapterId::Openai),
            "@ai-sdk/anthropic" => Some(AdapterId::Anthropic),
            "@ai-sdk/google" => Some(AdapterId::Gemini),
            "@ai-sdk/cohere" => Some(AdapterId::Cohere),
            _ => None,
        }
    }

    pub fn has_validator(connection: &ResolvedConnection, method: CredentialMethod) -> bool {
        !connection
            .rig_provider
            .as_ref()
            .is_some_and(|id| matches!(id.vendor(), "perplexity" | "huggingface"))
            && connection
                .auth_methods
                .iter()
                .any(|entry| entry.method == method && !entry.protocols.is_empty())
    }

    pub fn resolve(
        handle: &Handle,
        config: &ModelProviderConfig,
    ) -> Result<ResolvedConnection, InferenceError> {
        validate_attributes(handle, config)?;
        let brand = config
            .provider
            .clone()
            .unwrap_or_else(|| compatibility_brand(handle.as_str()).to_string());
        let recipe = recipe(&brand);
        let (adapter, rig_provider, protocols, auth_methods, default_base_url) = match recipe {
            Some(recipe) => {
                if let Some(explicit) = config.adapter
                    && explicit != recipe.adapter
                {
                    return Err(InferenceError::ConfigError(format!(
                        "providers.{handle}.adapter: brand '{brand}' requires adapter '{}'",
                        recipe.adapter.as_str()
                    )));
                }
                (
                    recipe.adapter,
                    recipe.rig_provider,
                    recipe.protocols,
                    recipe.auth_methods,
                    recipe.default_base_url,
                )
            }
            None => {
                let adapter = config.adapter.ok_or_else(|| {
                    InferenceError::ConfigError(format!(
                        "providers.{handle}.adapter: unknown brand '{brand}' requires a compiled adapter"
                    ))
                })?;
                let dynamic = dynamic_recipe(adapter);
                if config.base_url.is_none() && adapter != AdapterId::Ollama {
                    return Err(InferenceError::ConfigError(format!(
                        "providers.{handle}.base_url: unknown brand '{brand}' requires an endpoint"
                    )));
                }
                dynamic
            }
        };
        let endpoint = config.base_url.as_deref().or(default_base_url);
        let auth_methods = if (brand == "openai"
            && endpoint.map(|url| url.trim().trim_end_matches('/'))
                != Some("https://api.openai.com/v1"))
            || (brand == "github-copilot"
                && endpoint.map(|url| url.trim().trim_end_matches('/'))
                    != Some(crate::inference::provider::adapter::copilot::ENDPOINT))
        {
            API_KEY_OPENAI
        } else {
            auth_methods
        };
        if let Some(endpoint) = endpoint {
            let url = reqwest::Url::parse(endpoint.trim()).map_err(|_| {
                InferenceError::ConfigError(format!(
                    "providers.{handle}.base_url: expected an absolute HTTP(S) URL"
                ))
            })?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(InferenceError::ConfigError(format!(
                    "providers.{handle}.base_url: credentials, query parameters, and fragments are not supported"
                )));
            }
        }
        Ok(ResolvedConnection {
            handle: handle.clone(),
            brand,
            adapter,
            protocols,
            auth_methods,
            effective_base_url: endpoint.map(|url| normalize_endpoint(adapter, url)),
            rig_provider,
        })
    }
}

struct Recipe {
    adapter: AdapterId,
    rig_provider: Option<ProviderId>,
    protocols: &'static [ApiSurface],
    auth_methods: &'static [AuthMethodDescriptor],
    default_base_url: Option<&'static str>,
}

fn rig(selection: &str) -> Option<ProviderId> {
    Some(ProviderId::resolve(selection).expect("compiled Rig provider selection"))
}

fn recipe(brand: &str) -> Option<Recipe> {
    let recipe = match brand {
        "github-copilot" => Recipe::new(
            AdapterId::Openai,
            rig(rig_core::providers::openai::PROVIDER_NAME),
            OPENAI_PROTOCOLS,
            COPILOT_AUTH,
        )
        .endpoint(crate::inference::provider::adapter::copilot::ENDPOINT),
        "azure" => Recipe::new(
            AdapterId::Openai,
            rig(rig_core::providers::azure::PROVIDER_NAME),
            COMPLETIONS,
            API_KEY_COMPLETIONS,
        ),
        "amazon-bedrock" => Recipe::new(AdapterId::Bedrock, None, BEDROCK_PROTOCOLS, BEDROCK_AUTH),
        "openai" => Recipe::new(
            AdapterId::Openai,
            rig(rig_core::providers::openai::PROVIDER_NAME),
            OPENAI_PREFERRED_PROTOCOLS,
            OPENAI_AUTH,
        )
        .endpoint("https://api.openai.com/v1"),
        "anthropic" => Recipe::new(
            AdapterId::Anthropic,
            rig(rig_core::providers::anthropic::PROVIDER_NAME),
            ANTHROPIC_PROTOCOLS,
            API_KEY_ANTHROPIC,
        )
        .endpoint("https://api.anthropic.com"),
        "kimi-for-coding" => Recipe::new(
            AdapterId::Anthropic,
            rig(rig_core::providers::anthropic::PROVIDER_NAME),
            ANTHROPIC_PROTOCOLS,
            API_KEY_ANTHROPIC,
        )
        .endpoint("https://api.kimi.com/coding"),
        "ollama" => Recipe::new(
            AdapterId::Ollama,
            ProviderId::new(
                rig_core::providers::ollama::PROVIDER_NAME,
                rig_core::providers::registry::Format::OpenAi,
            ),
            OLLAMA_PROTOCOLS,
            ANONYMOUS_OLLAMA,
        )
        .endpoint("http://localhost:11434"),
        "groq" => Recipe::openai_compatible(rig(rig_core::providers::groq::PROVIDER_NAME))
            .endpoint("https://api.groq.com/openai/v1"),
        "openrouter" => {
            Recipe::openai_compatible(rig(rig_core::providers::openrouter::PROVIDER_NAME))
                .endpoint("https://openrouter.ai/api/v1")
        }
        "deepseek" => Recipe::openai_compatible(rig(rig_core::providers::deepseek::PROVIDER_NAME))
            .endpoint("https://api.deepseek.com"),
        "google" => Recipe::new(
            AdapterId::Gemini,
            rig(rig_core::providers::gemini::PROVIDER_NAME),
            GEMINI_PROTOCOLS,
            API_KEY_GEMINI,
        )
        .endpoint("https://generativelanguage.googleapis.com"),
        "cohere" => Recipe::new(
            AdapterId::Cohere,
            rig(rig_core::providers::cohere::PROVIDER_NAME),
            COHERE_PROTOCOLS,
            API_KEY_COHERE,
        )
        .endpoint("https://api.cohere.ai"),
        "mistral" => Recipe::openai_compatible(rig(rig_core::providers::mistral::PROVIDER_NAME))
            .endpoint("https://api.mistral.ai"),
        "perplexity" => {
            Recipe::openai_compatible(rig(rig_core::providers::perplexity::PROVIDER_NAME))
                .endpoint("https://api.perplexity.ai")
        }
        "togetherai" => {
            Recipe::openai_compatible(rig(rig_core::providers::together::PROVIDER_NAME))
                .endpoint("https://api.together.xyz")
        }
        "xai" => Recipe::openai_compatible(rig(rig_core::providers::openai::PROVIDER_NAME))
            .endpoint("https://api.x.ai/v1"),
        "hyperbolic" => {
            Recipe::openai_compatible(rig(rig_core::providers::hyperbolic::PROVIDER_NAME))
                .endpoint("https://api.hyperbolic.xyz")
        }
        "moonshotai" => Recipe::openai_compatible(ProviderId::new(
            rig_core::providers::moonshot::PROVIDER_NAME,
            rig_core::providers::registry::Format::OpenAi,
        ))
        .endpoint("https://api.moonshot.ai/v1"),
        "mira" => Recipe::openai_compatible(rig(rig_core::providers::mira::PROVIDER_NAME))
            .endpoint("https://api.mira.network"),
        "galadriel" => Recipe::openai_compatible(rig(rig_core::providers::openai::PROVIDER_NAME))
            .endpoint("https://api.galadriel.com/v1/verified"),
        "huggingface" => Recipe::new(
            AdapterId::Huggingface,
            rig(rig_core::providers::huggingface::PROVIDER_NAME),
            HUGGINGFACE_PROTOCOLS,
            API_KEY_HUGGINGFACE,
        )
        .endpoint("https://router.huggingface.co"),
        _ => return None,
    };
    Some(recipe)
}

impl Recipe {
    fn new(
        adapter: AdapterId,
        rig_provider: Option<ProviderId>,
        protocols: &'static [ApiSurface],
        auth_methods: &'static [AuthMethodDescriptor],
    ) -> Self {
        Self {
            adapter,
            rig_provider,
            protocols,
            auth_methods,
            default_base_url: None,
        }
    }

    fn openai_compatible(rig_provider: Option<ProviderId>) -> Self {
        Self::new(
            AdapterId::Openai,
            rig_provider,
            COMPLETIONS,
            API_KEY_COMPLETIONS,
        )
    }

    const fn endpoint(mut self, endpoint: &'static str) -> Self {
        self.default_base_url = Some(endpoint);
        self
    }
}

fn dynamic_recipe(
    adapter: AdapterId,
) -> (
    AdapterId,
    Option<ProviderId>,
    &'static [ApiSurface],
    &'static [AuthMethodDescriptor],
    Option<&'static str>,
) {
    match adapter {
        AdapterId::Openai => (
            adapter,
            rig(rig_core::providers::openai::PROVIDER_NAME),
            OPENAI_PROTOCOLS,
            API_KEY_OPENAI,
            None,
        ),
        AdapterId::Anthropic => (
            adapter,
            rig(rig_core::providers::anthropic::PROVIDER_NAME),
            ANTHROPIC_PROTOCOLS,
            API_KEY_ANTHROPIC,
            None,
        ),
        AdapterId::Gemini => (
            adapter,
            rig(rig_core::providers::gemini::PROVIDER_NAME),
            GEMINI_PROTOCOLS,
            API_KEY_GEMINI,
            None,
        ),
        AdapterId::Cohere => (
            adapter,
            rig(rig_core::providers::cohere::PROVIDER_NAME),
            COHERE_PROTOCOLS,
            API_KEY_COHERE,
            None,
        ),
        AdapterId::Ollama => (
            adapter,
            ProviderId::new(
                rig_core::providers::ollama::PROVIDER_NAME,
                rig_core::providers::registry::Format::OpenAi,
            ),
            OLLAMA_PROTOCOLS,
            ANONYMOUS_OLLAMA,
            Some("http://localhost:11434"),
        ),
        AdapterId::Huggingface => (
            adapter,
            rig(rig_core::providers::huggingface::PROVIDER_NAME),
            HUGGINGFACE_PROTOCOLS,
            API_KEY_HUGGINGFACE,
            None,
        ),
        AdapterId::Bedrock => (adapter, None, BEDROCK_PROTOCOLS, BEDROCK_AUTH, None),
    }
}

fn compatibility_brand(handle: &str) -> &str {
    match handle {
        "gemini" => "google",
        "together" => "togetherai",
        "moonshot" => "moonshotai",
        other => other,
    }
}

fn validate_attributes(
    handle: &Handle,
    config: &ModelProviderConfig,
) -> Result<(), InferenceError> {
    let bedrock = config.provider.as_deref().unwrap_or(handle.as_str()) == "amazon-bedrock"
        || config.adapter == Some(AdapterId::Bedrock);
    for (name, present) in [
        ("aws_profile", config.aws_profile.is_some()),
        ("aws_region", config.aws_region.is_some()),
        ("azure_credential", config.azure_credential.is_some()),
    ] {
        if present && !(bedrock && matches!(name, "aws_region" | "aws_profile")) {
            return Err(InferenceError::ConfigError(format!(
                "providers.{handle}.{name}: field is not supported by this provider recipe"
            )));
        }
    }
    if bedrock {
        for (name, value) in [
            ("aws_region", &config.aws_region),
            ("aws_profile", &config.aws_profile),
        ] {
            if value.as_ref().is_some_and(|value| {
                value.trim().is_empty()
                    || value != value.trim()
                    || value.chars().any(char::is_control)
            }) {
                return Err(InferenceError::ConfigError(format!(
                    "providers.{handle}.{name}: expected a nonempty selector without whitespace padding or control characters"
                )));
            }
        }
    }
    if config.provider.as_deref().unwrap_or(handle.as_str()) == "azure" {
        crate::inference::provider::adapter::azure::validate_attributes(handle, config)?;
    } else if let Some(name) = config.attributes.keys().next() {
        return Err(InferenceError::ConfigError(format!(
            "providers.{handle}.{name}: unknown provider attribute"
        )));
    }
    Ok(())
}

pub fn normalize_endpoint(adapter: AdapterId, input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    match adapter {
        AdapterId::Anthropic => anthropic::wire::normalize_base_url(trimmed),
        AdapterId::Openai => trimmed
            .strip_suffix("/chat/completions")
            .or_else(|| trimmed.strip_suffix("/responses"))
            .unwrap_or(trimmed)
            .trim_end_matches('/')
            .to_string(),
        _ => trimmed.to_string(),
    }
}

/// Compile an explicit connection without consulting environment credentials or
/// restricting model IDs to either catalog. Rig facts only guide serialization.
pub fn build_provider(
    resolved: &ResolvedConnection,
    config: &ModelProviderConfig,
    counter: &InferenceCounter,
) -> Result<ProviderConnection, InferenceError> {
    use crate::inference::protocol::http::WireClient;
    use crate::inference::provider::adapter;
    if resolved.brand == "github-copilot" {
        return Err(InferenceError::ConfigError(
            "Copilot requires a registered credential-refresh provider".into(),
        ));
    }
    if resolved.adapter == AdapterId::Bedrock {
        let provider = Arc::new(adapter::bedrock::BedrockProvider::new(
            config.clone(),
            counter.clone(),
        ));
        let discovery = provider.clone();
        let validation = provider.clone();
        return Ok(ProviderConnection {
            model: provider,
            admin: ProviderAdmin::default()
                .with_listing_check(move || {
                    let provider = discovery.clone();
                    async move { provider.list_models().await }
                })
                .with_validation(move || {
                    let provider = validation.clone();
                    async move { provider.validate_credentials().await }
                }),
        });
    }
    if resolved.adapter == AdapterId::Ollama {
        let connection = ollama::OllamaConfig::new().with_base_url(
            resolved
                .effective_base_url
                .as_deref()
                .expect("Ollama endpoint"),
        );
        let client = connection.clone().connect(WireClient::default());
        let discovery = connection.connect(adapter::discovery::http_client()?);
        return Ok(ProviderConnection {
            model: Arc::new(
                RigProvider::new(
                    move |id| client.native_completion(id).erase(),
                    counter.clone(),
                )
                .with_wire_transport(),
            ),
            admin: ProviderAdmin::default().with_live_check(move || {
                let client = discovery.clone();
                async move {
                    client
                        .list_models()
                        .await
                        .map_err(CredentialValidationError::from)
                }
            }),
        });
    }
    let key = require_api_key(resolved.handle.as_str(), config)?;
    let mut rig = resolved
        .rig_provider
        .as_ref()
        .and_then(|id| id.config(key))
        .expect("compiled recipe names a registered Rig provider");
    if let Some(endpoint) = &resolved.effective_base_url {
        rig = rig.with_base_url(endpoint);
    }
    if let ProviderConfig::OpenAi(connection) = &mut rig {
        connection.route = Some(openai::Route::Chat);
        if resolved.brand == "azure" {
            if resolved.effective_base_url.is_none() {
                return Err(InferenceError::ConfigError(
                    "Azure requires base_url for the resource endpoint".into(),
                ));
            }
            *connection = connection.clone().with_api_version(
                config
                    .attributes
                    .get("azure_api_version")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(adapter::azure::DEFAULT_API_VERSION),
            );
        }
        if resolved.adapter == AdapterId::Cohere {
            // CohereConfig::completion uses this compatibility root too.
            *connection = connection.clone().with_base_url(format!(
                "{}/compatibility/v1",
                connection.base_url.trim_end_matches('/')
            ));
        }
    }
    let admin = match resolved.rig_provider.as_ref().map(ProviderId::vendor) {
        Some(rig_core::providers::azure::PROVIDER_NAME) => adapter::azure::admin(resolved, config)?,
        Some(rig_core::providers::gemini::PROVIDER_NAME) => {
            adapter::gemini::admin(resolved, config)?
        }
        Some("openrouter") => adapter::openrouter::admin(resolved, config)?,
        Some("cohere") => adapter::cohere::admin(resolved, config)?,
        Some("together") => adapter::together::admin(resolved, config)?,
        Some("hyperbolic" | "huggingface") => {
            let directory = ProviderConfig::OpenAi(
                openai::OpenAIConfig::new(require_api_key(resolved.handle.as_str(), config)?)
                    .with_base_url(format!(
                        "{}/v1",
                        resolved
                            .effective_base_url
                            .as_deref()
                            .expect("provider endpoint")
                    )),
            );
            let admin = listing_admin(&directory)?;
            if resolved.adapter == AdapterId::Huggingface {
                // This public directory does not prove token validity.
                admin
                    .with_validation_check(|| async { Err(CredentialValidationError::Unsupported) })
            } else {
                admin
            }
        }
        Some("perplexity") => ProviderAdmin::default(),
        _ => listing_admin(&rig)?,
    };
    let http = DynHttpClient::new(if resolved.brand == "azure" {
        WireClient::without_redirects()
            .map_err(|_| InferenceError::ConfigError("Cannot build Azure HTTP client".into()))?
    } else {
        WireClient::default()
    });
    let selectable_route = resolved.adapter == AdapterId::Openai
        && rig.vendor() == "openai"
        && !matches!(resolved.brand.as_str(), "xai" | "galadriel");
    Ok(ProviderConnection {
        model: Arc::new(
            RigProvider::from_model_factory(
                move |model| {
                    let mut connection = rig.clone();
                    let responses = selectable_route
                        && matches!(
                            model.provider,
                            crate::config::ProviderModel::OpenAI {
                                api: Some(crate::config::OpenAiApi::Responses),
                                ..
                            }
                        );
                    if responses && let ProviderConfig::OpenAi(config) = &mut connection {
                        config.route = Some(openai::Route::Responses);
                    }
                    ResolvedModel::new(connection.completion_model(
                        &model.model_id,
                        http.clone(),
                        Default::default(),
                    ))
                },
                counter.clone(),
            )
            .with_wire_transport(),
        ),
        admin,
    })
}

fn listing_admin(config: &ProviderConfig) -> Result<ProviderAdmin, InferenceError> {
    let http = crate::inference::provider::adapter::discovery::http_client()?;
    // Listing remains a concrete provider operation; no callable model or
    // application catalog is used to infer account inventory.
    Ok(match config {
        ProviderConfig::OpenAi(config) => {
            let client = config.clone().connect(http);
            ProviderAdmin::default().with_live_check(move || {
                let client = client.clone();
                async move {
                    client
                        .list_models()
                        .await
                        .map_err(CredentialValidationError::from)
                }
            })
        }
        ProviderConfig::Anthropic(config) => {
            let client = config.clone().connect(http);
            ProviderAdmin::default().with_live_check(move || {
                let client = client.clone();
                async move {
                    client
                        .list_models()
                        .await
                        .map_err(CredentialValidationError::from)
                }
            })
        }
        ProviderConfig::Gemini(_) => unreachable!("Gemini has a distinct invalid-key probe"),
    })
}

pub fn require_api_key(
    provider: &str,
    config: &ModelProviderConfig,
) -> Result<String, InferenceError> {
    config
        .api_key
        .clone()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            InferenceError::ConfigError(format!(
                "Provider '{provider}' requires an api_key but none was provided"
            ))
        })
}

pub fn accepts_openrouter(connection: &ResolvedConnection) -> bool {
    connection.brand == "openrouter"
        && connection.rig_provider == rig(rig_core::providers::openrouter::PROVIDER_NAME)
        && connection.effective_base_url.as_deref() == Some("https://openrouter.ai/api/v1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(value: &str) -> Handle {
        Handle::try_new(value).unwrap()
    }

    #[test]
    fn every_recipe_resolves_registered_identity_and_keeps_frona_adapter_names() {
        let expected = [
            ("github-copilot", "openai"),
            ("azure", "openai"),
            ("amazon-bedrock", "bedrock"),
            ("openai", "openai"),
            ("anthropic", "anthropic"),
            ("kimi-for-coding", "anthropic"),
            ("ollama", "ollama"),
            ("groq", "groq"),
            ("openrouter", "openrouter"),
            ("deepseek", "deepseek"),
            ("google", "gemini"),
            ("cohere", "cohere"),
            ("mistral", "mistral"),
            ("perplexity", "perplexity"),
            ("togetherai", "together"),
            ("xai", "xai"),
            ("hyperbolic", "hyperbolic"),
            ("moonshotai", "moonshot"),
            ("mira", "mira"),
            ("galadriel", "galadriel"),
            ("huggingface", "huggingface"),
        ];
        assert_eq!(
            expected.iter().map(|(brand, _)| *brand).collect::<Vec<_>>(),
            ProviderPlatform::built_in_brands()
        );
        for (brand, adapter) in expected {
            let config = ModelProviderConfig {
                provider: Some(brand.into()),
                ..Default::default()
            };
            let resolved = ProviderPlatform::resolve(&handle("named-account"), &config).unwrap();
            assert_eq!(resolved.handle.as_str(), "named-account");
            assert_eq!(resolved.brand, brand);
            assert_eq!(resolved.request_adapter_name(), adapter, "{brand}");
            if brand == "amazon-bedrock" {
                assert!(resolved.rig_provider.is_none());
            } else {
                let selection = resolved.rig_provider.unwrap();
                let native = selection
                    .config("fixture-key")
                    .expect("registered configuration");
                assert_eq!(native.id(), Some(selection), "{brand}");
            }
        }
        let custom = ProviderPlatform::resolve(
            &handle("custom-google"),
            &ModelProviderConfig {
                provider: Some("private-gateway".into()),
                adapter: Some(AdapterId::Gemini),
                base_url: Some("https://example.test/gateway".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(custom.request_adapter_name(), "gemini");
        assert_eq!(
            custom.rig_provider.unwrap().vendor(),
            rig_core::providers::gemini::PROVIDER_NAME
        );
    }

    #[test]
    fn resolves_two_named_openai_connections_through_one_recipe() {
        for name in ["openai-prod", "openai-stage"] {
            let resolved = ProviderPlatform::resolve(
                &handle(name),
                &ModelProviderConfig {
                    provider: Some("openai".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(resolved.adapter, AdapterId::Openai);
            assert_eq!(
                resolved.rig_provider,
                rig(rig_core::providers::openai::PROVIDER_NAME)
            );
        }
    }

    #[test]
    fn vertex_is_deferred_without_disabling_gemini() {
        for npm in ["@ai-sdk/google-vertex", "@ai-sdk/google-vertex/anthropic"] {
            assert_eq!(ProviderPlatform::adapter_for_npm(npm), None);
        }
        for brand in ["google-vertex", "google-vertex-anthropic"] {
            assert!(!ProviderPlatform::has_recipe(brand));
            assert!(!ProviderPlatform::built_in_brands().contains(&brand));
        }
        assert!(serde_json::from_str::<AdapterId>(r#""vertex""#).is_err());
        assert!(serde_json::from_str::<ApiSurface>(r#""google-vertex-generate-content""#).is_err());
        let gemini =
            ProviderPlatform::resolve(&handle("google"), &ModelProviderConfig::default()).unwrap();
        assert_eq!(gemini.adapter, AdapterId::Gemini);
        assert!(!gemini.protocols.is_empty());
    }

    #[test]
    fn resolves_dynamic_generic_openai_without_catalogs() {
        let resolved = ProviderPlatform::resolve(
            &handle("internal-gateway"),
            &ModelProviderConfig {
                provider: Some("internal".into()),
                adapter: Some(AdapterId::Openai),
                base_url: Some(" https://models.example/v1/chat/completions/ ".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            resolved.rig_provider,
            rig(rig_core::providers::openai::PROVIDER_NAME)
        );
        assert_eq!(
            resolved.effective_base_url.as_deref(),
            Some("https://models.example/v1")
        );
    }

    #[test]
    fn rejects_foreign_fields_unknown_adapters_and_brand_mismatch() {
        let foreign = ProviderPlatform::resolve(
            &handle("deepseek-prod"),
            &ModelProviderConfig {
                provider: Some("deepseek".into()),
                aws_profile: Some("prod".into()),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(foreign.contains("providers.deepseek-prod.aws_profile"));

        let mismatch = ProviderPlatform::resolve(
            &handle("openai-prod"),
            &ModelProviderConfig {
                provider: Some("openai".into()),
                adapter: Some(AdapterId::Anthropic),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(mismatch.contains("requires adapter 'openai'"));
    }

    #[test]
    fn kimi_recipe_normalizes_the_anthropic_endpoint() {
        let resolved = ProviderPlatform::resolve(
            &handle("kimi-code"),
            &ModelProviderConfig {
                provider: Some("kimi-for-coding".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.adapter, AdapterId::Anthropic);
        assert_eq!(
            resolved.effective_base_url.as_deref(),
            Some("https://api.kimi.com/coding")
        );
    }
}
