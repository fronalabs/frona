use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::core::state::AppState;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/config/providers/{id}/models",
        get(list_provider_models),
    )
}

#[derive(Debug, Clone, Serialize)]
struct ModelInfo {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// Provider base URLs & endpoint config
// ---------------------------------------------------------------------------

fn default_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("https://api.openai.com/v1"),
        "anthropic" => Some("https://api.anthropic.com"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "deepseek" => Some("https://api.deepseek.com"),
        "gemini" => Some("https://generativelanguage.googleapis.com"),
        "cohere" => Some("https://api.cohere.ai"),
        "mistral" => Some("https://api.mistral.ai"),
        "perplexity" => Some("https://api.perplexity.ai"),
        "together" => Some("https://api.together.xyz"),
        "xai" => Some("https://api.x.ai"),
        "hyperbolic" => Some("https://api.hyperbolic.xyz"),
        "moonshot" => Some("https://api.moonshot.cn/v1"),
        "mira" => Some("https://api.mira.network"),
        "galadriel" => Some("https://api.galadriel.com/v1/verified"),
        "huggingface" => Some("https://router.huggingface.co"),
        "ollama" => Some("http://localhost:11434"),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ResponseFormat {
    OpenAi,
    OpenRouter,
    Gemini,
    Ollama,
}

fn models_endpoint(provider: &str) -> Option<(&'static str, ResponseFormat)> {
    match provider {
        "openrouter" => Some(("/models", ResponseFormat::OpenRouter)),
        "openai" | "groq" | "cohere" | "mistral" | "together" | "xai" | "hyperbolic"
        | "moonshot" | "deepseek" => Some(("/models", ResponseFormat::OpenAi)),
        "anthropic" => Some(("/v1/models", ResponseFormat::OpenAi)),
        "perplexity" => Some(("/models", ResponseFormat::OpenAi)),
        "gemini" => Some(("/v1beta/models", ResponseFormat::Gemini)),
        "ollama" => Some(("/api/tags", ResponseFormat::Ollama)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Extract models from provider response
// ---------------------------------------------------------------------------

fn extract_models(
    body: &serde_json::Value,
    format: ResponseFormat,
    provider: &str,
) -> Vec<ModelInfo> {
    let mut models = match format {
        ResponseFormat::OpenAi => extract_openai(body),
        ResponseFormat::OpenRouter => extract_openrouter(body),
        ResponseFormat::Gemini => extract_gemini(body),
        ResponseFormat::Ollama => extract_ollama(body),
    };

    // Enrich with hardcoded limits for models missing them
    for model in &mut models {
        if (model.context_window.is_none() || model.max_tokens.is_none())
            && let Some((ctx, max)) = hardcoded_limits(provider, &model.id)
        {
            if model.context_window.is_none() {
                model.context_window = Some(ctx);
            }
            if model.max_tokens.is_none() {
                model.max_tokens = Some(max);
            }
        }
    }

    models
}

fn extract_openai(body: &serde_json::Value) -> Vec<ModelInfo> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    // Anthropic uses "display_name", others may not have one
                    let name = m
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    Some(ModelInfo {
                        id,
                        name,
                        context_window: None,
                        max_tokens: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_openrouter(body: &serde_json::Value) -> Vec<ModelInfo> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    let name = m.get("name").and_then(|v| v.as_str()).map(String::from);
                    let context_window = m
                        .get("context_length")
                        .and_then(|v| v.as_u64());
                    let max_tokens = m
                        .get("top_provider")
                        .and_then(|tp| tp.get("max_completion_tokens"))
                        .and_then(|v| v.as_u64());
                    Some(ModelInfo {
                        id,
                        name,
                        context_window,
                        max_tokens,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_gemini(body: &serde_json::Value) -> Vec<ModelInfo> {
    body.get("models")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let raw_name = m.get("name")?.as_str()?;
                    let id = raw_name.strip_prefix("models/").unwrap_or(raw_name).to_string();
                    let name = m
                        .get("displayName")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let context_window = m.get("inputTokenLimit").and_then(|v| v.as_u64());
                    let max_tokens = m.get("outputTokenLimit").and_then(|v| v.as_u64());
                    Some(ModelInfo {
                        id,
                        name,
                        context_window,
                        max_tokens,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_ollama(body: &serde_json::Value) -> Vec<ModelInfo> {
    body.get("models")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("name")?.as_str()?.to_string();
                    Some(ModelInfo {
                        id,
                        name: None,
                        context_window: None,
                        max_tokens: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Hardcoded fallback limits: (context_window, max_output_tokens)
// ---------------------------------------------------------------------------

fn hardcoded_limits(provider: &str, model_id: &str) -> Option<(u64, u64)> {
    // Try exact match first, then prefix match.
    // Order matters: more specific prefixes must come before shorter ones
    // (e.g. "gpt-4o-mini" before "gpt-4o", "grok-3-mini" before "grok-3").
    let table: &[(&str, &str, u64, u64)] = &[
        // Anthropic
        ("anthropic", "claude-opus-4", 200_000, 32_000),
        ("anthropic", "claude-sonnet-4", 200_000, 64_000),
        ("anthropic", "claude-haiku-4", 200_000, 64_000),
        ("anthropic", "claude-3-5-sonnet", 200_000, 8_192),
        ("anthropic", "claude-3-5-haiku", 200_000, 8_192),
        ("anthropic", "claude-3-opus", 200_000, 4_096),
        ("anthropic", "claude-3-sonnet", 200_000, 4_096),
        ("anthropic", "claude-3-haiku", 200_000, 4_096),
        // OpenAI
        ("openai", "gpt-4.1-mini", 1_047_576, 32_768),
        ("openai", "gpt-4.1-nano", 1_047_576, 32_768),
        ("openai", "gpt-4.1", 1_047_576, 32_768),
        ("openai", "gpt-4o-mini", 128_000, 16_384),
        ("openai", "gpt-4o", 128_000, 16_384),
        ("openai", "gpt-4-turbo", 128_000, 4_096),
        ("openai", "gpt-4", 8_192, 8_192),
        ("openai", "gpt-3.5-turbo", 16_385, 4_096),
        ("openai", "o1-mini", 128_000, 65_536),
        ("openai", "o1-preview", 128_000, 32_768),
        ("openai", "o1", 200_000, 100_000),
        ("openai", "o3-mini", 200_000, 100_000),
        ("openai", "o3", 200_000, 100_000),
        ("openai", "o4-mini", 200_000, 100_000),
        // DeepSeek
        ("deepseek", "deepseek-chat", 128_000, 8_192),
        ("deepseek", "deepseek-reasoner", 128_000, 8_192),
        // Groq
        ("groq", "llama-3.3-70b-versatile", 128_000, 32_768),
        ("groq", "llama-3.1-8b-instant", 131_072, 8_192),
        ("groq", "gemma2-9b-it", 8_192, 8_192),
        // Mistral
        ("mistral", "mistral-large-latest", 128_000, 8_192),
        ("mistral", "mistral-small-latest", 128_000, 8_192),
        ("mistral", "codestral-latest", 256_000, 8_192),
        ("mistral", "open-mistral-nemo", 128_000, 8_192),
        ("mistral", "mistral-saba-latest", 32_000, 8_192),
        ("mistral", "pixtral-large-latest", 128_000, 8_192),
        // Cohere
        ("cohere", "command-a", 256_000, 8_000),
        ("cohere", "command-r-plus", 128_000, 4_096),
        ("cohere", "command-r", 128_000, 4_096),
        // xAI
        ("xai", "grok-3-mini", 131_072, 16_384),
        ("xai", "grok-3", 131_072, 16_384),
        ("xai", "grok-2-mini", 131_072, 8_192),
        ("xai", "grok-2", 131_072, 8_192),
        // Perplexity
        ("perplexity", "sonar-pro", 200_000, 8_192),
        ("perplexity", "sonar-reasoning-pro", 127_072, 8_192),
        ("perplexity", "sonar-reasoning", 127_072, 8_192),
        ("perplexity", "sonar-deep-research", 127_072, 8_192),
        ("perplexity", "sonar", 127_072, 8_192),
        // Gemini (fallback for when API doesn't return limits)
        ("gemini", "gemini-2.5-pro", 1_048_576, 65_536),
        ("gemini", "gemini-2.5-flash", 1_048_576, 65_536),
        ("gemini", "gemini-2.0-flash", 1_048_576, 8_192),
        ("gemini", "gemini-1.5-pro", 2_097_152, 8_192),
        ("gemini", "gemini-1.5-flash", 1_048_576, 8_192),
        // Together (common models)
        ("together", "meta-llama/Llama-3.3-70B-Instruct-Turbo", 128_000, 64_000),
        ("together", "meta-llama/Llama-4-Scout", 10_000_000, 64_000),
        ("together", "deepseek-ai/DeepSeek-V3", 128_000, 64_000),
        ("together", "deepseek-ai/DeepSeek-R1", 128_000, 64_000),
        ("together", "Qwen/Qwen3", 1_000_000, 64_000),
    ];

    // Exact match on provider + model prefix
    for &(p, prefix, ctx, max) in table {
        if p == provider && model_id.starts_with(prefix) {
            return Some((ctx, max));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Route handler
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ListModelsQuery {
    api_key: Option<String>,
    base_url: Option<String>,
}

async fn list_provider_models(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ListModelsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let saved = state.config.providers.get(&provider_id);
    let api_key = query
        .api_key
        .or_else(|| saved.and_then(|p| p.api_key.clone()));
    let base_url = query
        .base_url
        .or_else(|| saved.and_then(|p| p.base_url.clone()));

    let (endpoint_path, format) = models_endpoint(&provider_id).ok_or_else(|| {
        ApiError(crate::core::error::AppError::Validation(format!(
            "Model listing not supported for provider '{provider_id}'"
        )))
    })?;

    let base = base_url
        .as_deref()
        .or_else(|| default_base_url(&provider_id))
        .ok_or_else(|| {
            ApiError(crate::core::error::AppError::Validation(format!(
                "No base URL for provider '{provider_id}'"
            )))
        })?;

    let url = format!("{}{}", base.trim_end_matches('/'), endpoint_path);

    let client = reqwest::Client::new();
    let mut req = client.get(&url);

    if let Some(ref key) = api_key
        && !key.is_empty()
    {
        if provider_id == "anthropic" {
            req = req.header("x-api-key", key);
            req = req.header("anthropic-version", "2023-06-01");
        } else {
            req = req.bearer_auth(key);
        }
    }

    let resp = req
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            ApiError(crate::core::error::AppError::Internal(format!(
                "Failed to fetch models from {provider_id} ({url}): {e:?}"
            )))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError(crate::core::error::AppError::Internal(format!(
            "Provider '{provider_id}' returned {status}: {body}"
        ))));
    }

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        ApiError(crate::core::error::AppError::Internal(format!(
            "Failed to parse models response from {provider_id}: {e}"
        )))
    })?;

    let models = extract_models(&body, format, &provider_id);

    Ok(Json(serde_json::json!({ "models": models })))
}
