use std::sync::Arc;

use rig_core::client::ModelListingClient;
use rig_core::client::Nothing;
use rig_core::providers::{
    anthropic, deepseek, gemini, groq, mira, mistral, moonshot, ollama, openai, openrouter,
};

use crate::core::Handle;
use crate::core::config::{AdapterId, ApiSurface, ModelProviderConfig};
use crate::inference::credential::store::CredentialMethod;

use crate::inference::error::InferenceError;
use crate::inference::protocol::hooks;
use crate::inference::provider::{
    CredentialValidationError, InferenceCounter, ModelProvider, OpenAiProvider, RigProvider,
};

#[derive(Debug, Clone, Copy)]
pub struct AuthMethodDescriptor {
    pub id: &'static str,
    pub method: CredentialMethod,
    pub priority: u16,
    pub protocols: &'static [ApiSurface],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FactoryKind {
    Copilot,
    Azure,
    Bedrock,
    OpenAi,
    Anthropic,
    Ollama,
    Groq,
    OpenRouter,
    DeepSeek,
    Gemini,
    Cohere,
    Mistral,
    Perplexity,
    Together,
    Xai,
    Hyperbolic,
    Moonshot,
    Mira,
    Galadriel,
    HuggingFace,
    GenericOpenAi,
}

#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub handle: Handle,
    pub brand: String,
    pub adapter: AdapterId,
    pub protocols: &'static [ApiSurface],
    pub auth_methods: &'static [AuthMethodDescriptor],
    pub effective_base_url: Option<String>,
    pub(crate) factory: FactoryKind,
}

impl ResolvedConnection {
    pub(crate) fn supports_model_protocol(&self, model: &str, api: ApiSurface) -> bool {
        self.protocols.contains(&api)
            && (self.factory != FactoryKind::Copilot
                || crate::inference::provider::adapter::copilot::protocol(model) == api)
    }

    pub(crate) fn supports_catalog_route(
        &self,
        model: &frona_model_catalog::catalog::ModelEntry,
    ) -> bool {
        self.factory != FactoryKind::Azure
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

    pub(crate) const fn request_adapter_name(&self) -> &'static str {
        match self.factory {
            FactoryKind::Copilot => "openai",
            FactoryKind::Azure => "openai",
            FactoryKind::Bedrock => "bedrock",
            FactoryKind::OpenAi | FactoryKind::GenericOpenAi => "openai",
            FactoryKind::Anthropic => "anthropic",
            FactoryKind::Ollama => "ollama",
            FactoryKind::Groq => "groq",
            FactoryKind::OpenRouter => "openrouter",
            FactoryKind::DeepSeek => "deepseek",
            FactoryKind::Gemini => "gemini",
            FactoryKind::Cohere => "cohere",
            FactoryKind::Mistral => "mistral",
            FactoryKind::Perplexity => "perplexity",
            FactoryKind::Together => "together",
            FactoryKind::Xai => "xai",
            FactoryKind::Hyperbolic => "hyperbolic",
            FactoryKind::Moonshot => "moonshot",
            FactoryKind::Mira => "mira",
            FactoryKind::Galadriel => "galadriel",
            FactoryKind::HuggingFace => "huggingface",
        }
    }
}

pub struct ProviderPlatform;

const COMPLETIONS: &[ApiSurface] = &[ApiSurface::Completions];
const OPENAI_PROTOCOLS: &[ApiSurface] = &[ApiSurface::Completions, ApiSurface::Responses];
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
        !matches!(
            connection.factory,
            FactoryKind::Perplexity | FactoryKind::HuggingFace
        ) && connection
            .auth_methods
            .iter()
            .any(|entry| entry.method == method && !entry.protocols.is_empty())
    }

    pub fn resolve(
        handle: &Handle,
        config: &ModelProviderConfig,
    ) -> Result<ResolvedConnection, InferenceError> {
        if config.credential_id.is_some()
            && (config.api_key.is_some()
                || config.azure_credential.is_some()
                || config.aws_profile.is_some())
        {
            return Err(InferenceError::ConfigError(
                "credential_id conflicts with another explicit authentication source".into(),
            ));
        }
        validate_attributes(handle, config)?;
        let brand = config
            .provider
            .clone()
            .unwrap_or_else(|| compatibility_brand(handle.as_str()).to_string());
        let recipe = recipe(&brand);
        let (adapter, factory, protocols, auth_methods, default_base_url) = match recipe {
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
                    recipe.factory,
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
        let auth_methods = if (factory == FactoryKind::OpenAi
            && endpoint.map(|url| url.trim().trim_end_matches('/'))
                != Some("https://api.openai.com/v1"))
            || (factory == FactoryKind::Copilot
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
            factory,
        })
    }
}

struct Recipe {
    adapter: AdapterId,
    factory: FactoryKind,
    protocols: &'static [ApiSurface],
    auth_methods: &'static [AuthMethodDescriptor],
    default_base_url: Option<&'static str>,
}

fn recipe(brand: &str) -> Option<Recipe> {
    let recipe =
        match brand {
            "github-copilot" => Recipe::new(
                AdapterId::Openai,
                FactoryKind::Copilot,
                OPENAI_PROTOCOLS,
                COPILOT_AUTH,
            )
            .endpoint(crate::inference::provider::adapter::copilot::ENDPOINT),
            "azure" => Recipe::new(
                AdapterId::Openai,
                FactoryKind::Azure,
                COMPLETIONS,
                API_KEY_COMPLETIONS,
            ),
            "amazon-bedrock" => Recipe::new(
                AdapterId::Bedrock,
                FactoryKind::Bedrock,
                BEDROCK_PROTOCOLS,
                BEDROCK_AUTH,
            ),
            "openai" => Recipe::new(
                AdapterId::Openai,
                FactoryKind::OpenAi,
                OPENAI_PROTOCOLS,
                OPENAI_AUTH,
            )
            .endpoint("https://api.openai.com/v1"),
            "anthropic" => Recipe::new(
                AdapterId::Anthropic,
                FactoryKind::Anthropic,
                ANTHROPIC_PROTOCOLS,
                API_KEY_ANTHROPIC,
            )
            .endpoint("https://api.anthropic.com"),
            "kimi-for-coding" => Recipe::new(
                AdapterId::Anthropic,
                FactoryKind::Anthropic,
                ANTHROPIC_PROTOCOLS,
                API_KEY_ANTHROPIC,
            )
            .endpoint("https://api.kimi.com/coding"),
            "ollama" => Recipe::new(
                AdapterId::Ollama,
                FactoryKind::Ollama,
                OLLAMA_PROTOCOLS,
                ANONYMOUS_OLLAMA,
            )
            .endpoint("http://localhost:11434"),
            "groq" => Recipe::openai_compatible(FactoryKind::Groq)
                .endpoint("https://api.groq.com/openai/v1"),
            "openrouter" => Recipe::openai_compatible(FactoryKind::OpenRouter)
                .endpoint("https://openrouter.ai/api/v1"),
            "deepseek" => Recipe::openai_compatible(FactoryKind::DeepSeek)
                .endpoint("https://api.deepseek.com"),
            "google" => Recipe::new(
                AdapterId::Gemini,
                FactoryKind::Gemini,
                GEMINI_PROTOCOLS,
                API_KEY_GEMINI,
            )
            .endpoint("https://generativelanguage.googleapis.com"),
            "cohere" => Recipe::new(
                AdapterId::Cohere,
                FactoryKind::Cohere,
                COHERE_PROTOCOLS,
                API_KEY_COHERE,
            )
            .endpoint("https://api.cohere.ai"),
            "mistral" => {
                Recipe::openai_compatible(FactoryKind::Mistral).endpoint("https://api.mistral.ai")
            }
            "perplexity" => Recipe::openai_compatible(FactoryKind::Perplexity)
                .endpoint("https://api.perplexity.ai"),
            "togetherai" => Recipe::openai_compatible(FactoryKind::Together)
                .endpoint("https://api.together.xyz"),
            "xai" => Recipe::openai_compatible(FactoryKind::Xai).endpoint("https://api.x.ai/v1"),
            "hyperbolic" => Recipe::openai_compatible(FactoryKind::Hyperbolic)
                .endpoint("https://api.hyperbolic.xyz"),
            "moonshotai" => Recipe::openai_compatible(FactoryKind::Moonshot)
                .endpoint("https://api.moonshot.ai/v1"),
            "mira" => {
                Recipe::openai_compatible(FactoryKind::Mira).endpoint("https://api.mira.network")
            }
            "galadriel" => Recipe::openai_compatible(FactoryKind::Galadriel)
                .endpoint("https://api.galadriel.com/v1/verified"),
            "huggingface" => Recipe::new(
                AdapterId::Huggingface,
                FactoryKind::HuggingFace,
                HUGGINGFACE_PROTOCOLS,
                API_KEY_HUGGINGFACE,
            )
            .endpoint("https://router.huggingface.co"),
            _ => return None,
        };
    Some(recipe)
}

impl Recipe {
    const fn new(
        adapter: AdapterId,
        factory: FactoryKind,
        protocols: &'static [ApiSurface],
        auth_methods: &'static [AuthMethodDescriptor],
    ) -> Self {
        Self {
            adapter,
            factory,
            protocols,
            auth_methods,
            default_base_url: None,
        }
    }

    const fn openai_compatible(factory: FactoryKind) -> Self {
        Self::new(AdapterId::Openai, factory, COMPLETIONS, API_KEY_COMPLETIONS)
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
    FactoryKind,
    &'static [ApiSurface],
    &'static [AuthMethodDescriptor],
    Option<&'static str>,
) {
    match adapter {
        AdapterId::Openai => (
            adapter,
            FactoryKind::GenericOpenAi,
            OPENAI_PROTOCOLS,
            API_KEY_OPENAI,
            None,
        ),
        AdapterId::Anthropic => (
            adapter,
            FactoryKind::Anthropic,
            ANTHROPIC_PROTOCOLS,
            API_KEY_ANTHROPIC,
            None,
        ),
        AdapterId::Gemini => (
            adapter,
            FactoryKind::Gemini,
            GEMINI_PROTOCOLS,
            API_KEY_GEMINI,
            None,
        ),
        AdapterId::Cohere => (
            adapter,
            FactoryKind::Cohere,
            COHERE_PROTOCOLS,
            API_KEY_COHERE,
            None,
        ),
        AdapterId::Ollama => (
            adapter,
            FactoryKind::Ollama,
            OLLAMA_PROTOCOLS,
            ANONYMOUS_OLLAMA,
            Some("http://localhost:11434"),
        ),
        AdapterId::Huggingface => (
            adapter,
            FactoryKind::HuggingFace,
            HUGGINGFACE_PROTOCOLS,
            API_KEY_HUGGINGFACE,
            None,
        ),
        AdapterId::Bedrock => (
            adapter,
            FactoryKind::Bedrock,
            BEDROCK_PROTOCOLS,
            BEDROCK_AUTH,
            None,
        ),
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
        AdapterId::Anthropic => anthropic::client::normalize_anthropic_base_url(trimmed),
        AdapterId::Openai => trimmed
            .strip_suffix("/chat/completions")
            .or_else(|| trimmed.strip_suffix("/responses"))
            .unwrap_or(trimmed)
            .trim_end_matches('/')
            .to_string(),
        _ => trimmed.to_string(),
    }
}

macro_rules! build_listable_key_client {
    ($name:expr, $config:expr, $endpoint:expr, $module:ident, $counter:expr) => {{
        let key = require_api_key($name, $config)?;
        let mut builder = $module::Client::builder()
            .http_client(crate::inference::protocol::http::WireClient::default())
            .api_key(&key);
        if let Some(url) = $endpoint {
            builder = builder.base_url(url);
        }
        let client = builder
            .build()
            .map_err(|error| config_error($name, error))?;
        let validation_client = client.clone();
        Ok(Arc::new(
            RigProvider::new(client, $counter.clone())
                .with_wire_transport()
                .with_live_check(move || {
                    let client = validation_client.clone();
                    async move {
                        client
                            .list_models()
                            .await
                            .map_err(CredentialValidationError::from)
                    }
                }),
        ) as Arc<dyn ModelProvider>)
    }};
}

pub(crate) fn build_provider(
    resolved: &ResolvedConnection,
    config: &ModelProviderConfig,
    counter: &InferenceCounter,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    let name = resolved.handle.as_str();
    let endpoint = resolved.effective_base_url.as_deref();
    match resolved.factory {
        FactoryKind::Copilot => Ok(Arc::new(
            crate::inference::provider::adapter::copilot::SuppliedTokenProvider::new(
                config,
                endpoint.expect("Copilot recipe endpoint"),
                counter.clone(),
            )?,
        )),
        FactoryKind::Azure => {
            crate::inference::provider::adapter::azure::build(resolved, config, counter)
        }
        FactoryKind::Bedrock => Ok(Arc::new(
            crate::inference::provider::adapter::bedrock::BedrockProvider::new(
                config.clone(),
                counter.clone(),
            ),
        )),
        FactoryKind::OpenAi | FactoryKind::GenericOpenAi => {
            let key = require_api_key(name, config)?;
            let mut completions = openai::CompletionsClient::builder()
                .http_client(crate::inference::protocol::http::WireClient::default())
                .api_key(&key);
            let mut responses = openai::Client::builder()
                .http_client(crate::inference::protocol::http::WireClient::default())
                .api_key(&key);
            if let Some(url) = endpoint {
                completions = completions.base_url(url);
                responses = responses.base_url(url);
            }
            Ok(Arc::new(
                OpenAiProvider::new(
                    completions
                        .build()
                        .map_err(|error| config_error(name, error))?,
                    responses
                        .build()
                        .map_err(|error| config_error(name, error))?,
                    counter.clone(),
                )
                .with_wire_transport(),
            ))
        }
        FactoryKind::Anthropic => {
            build_listable_key_client!(name, config, endpoint, anthropic, counter)
        }
        FactoryKind::Groq => build_listable_key_client!(name, config, endpoint, groq, counter),
        FactoryKind::OpenRouter => {
            build_listable_key_client!(name, config, endpoint, openrouter, counter)
        }
        FactoryKind::DeepSeek => {
            build_listable_key_client!(name, config, endpoint, deepseek, counter)
        }
        FactoryKind::Gemini => build_listable_key_client!(name, config, endpoint, gemini, counter),
        FactoryKind::Cohere => {
            crate::inference::provider::adapter::cohere::build(resolved, config, counter)
        }
        FactoryKind::Mistral => {
            build_listable_key_client!(name, config, endpoint, mistral, counter)
        }
        FactoryKind::Perplexity => {
            crate::inference::provider::adapter::perplexity::build(resolved, config, counter)
        }
        FactoryKind::Together => {
            crate::inference::provider::adapter::together::build(resolved, config, counter)
        }
        FactoryKind::Hyperbolic => {
            crate::inference::provider::adapter::hyperbolic::build(resolved, config, counter)
        }
        FactoryKind::Moonshot => {
            build_listable_key_client!(name, config, endpoint, moonshot, counter)
        }
        FactoryKind::Mira => build_listable_key_client!(name, config, endpoint, mira, counter),
        FactoryKind::HuggingFace => {
            crate::inference::provider::adapter::huggingface::build(resolved, config, counter)
        }
        FactoryKind::Galadriel | FactoryKind::Xai => {
            let key = require_api_key(name, config)?;
            let client = openai::CompletionsClient::builder()
                .http_client(crate::inference::protocol::http::WireClient::default())
                .api_key(&key)
                .base_url(endpoint.expect("Galadriel recipe has an endpoint"))
                .build()
                .map_err(|error| config_error(name, error))?;
            Ok(Arc::new(
                RigProvider::new(client.clone(), counter.clone())
                    .with_wire_transport()
                    .with_live_check(move || {
                        let client = client.clone();
                        async move {
                            client
                                .list_models()
                                .await
                                .map_err(CredentialValidationError::from)
                        }
                    })
                    .with_hook(hooks::openai),
            ))
        }
        FactoryKind::Ollama => {
            let mut builder = ollama::Client::builder()
                .http_client(crate::inference::protocol::http::WireClient::default())
                .api_key(Nothing);
            if let Some(url) = endpoint {
                builder = builder.base_url(url);
            }
            let client = builder.build().map_err(|error| config_error(name, error))?;
            let validation_client = client.clone();
            Ok(Arc::new(
                RigProvider::new(client, counter.clone())
                    .with_wire_transport()
                    .with_live_check(move || {
                        let client = validation_client.clone();
                        async move {
                            client
                                .list_models()
                                .await
                                .map_err(CredentialValidationError::from)
                        }
                    })
                    .with_hook(hooks::ollama),
            ))
        }
    }
}

pub(crate) fn require_api_key(
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

fn config_error(provider: &str, error: impl std::fmt::Display) -> InferenceError {
    InferenceError::ConfigError(format!("{provider}: {error}"))
}

pub(crate) fn accepts_openrouter(connection: &ResolvedConnection) -> bool {
    connection.brand == "openrouter"
        && connection.factory == FactoryKind::OpenRouter
        && connection.effective_base_url.as_deref() == Some("https://openrouter.ai/api/v1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(value: &str) -> Handle {
        Handle::try_new(value).unwrap()
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
            assert_eq!(resolved.factory, FactoryKind::OpenAi);
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
        assert_eq!(resolved.factory, FactoryKind::GenericOpenAi);
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
