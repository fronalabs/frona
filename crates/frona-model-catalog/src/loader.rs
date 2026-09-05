//! Parse models.dev provider and model authoring metadata. Local selection and
//! network/cache lifecycle are owned by `sources`.
//!
//! Source: `https://models.dev/catalog.json` - community-maintained at
//! `github.com/anomalyco/models.dev`. The `catalog` endpoint combines:
//! - `providers.<provider>.models.<model>` - provider serving details (cost,
//!   limits, capability flags) - the part we persist into the catalog.
//! - `models.<provider/model>` - provider-agnostic facts (benchmarks, weights,
//!   licenses) - unused for now but kept around so we don't need a second
//!   fetch when we want to surface those later.
//!
//! `ModelEntry` mirrors the upstream shape exactly (cost/limit/modalities are
//! nested structs); the per-1M-token -> per-token rescale happens at the
//! `ModelEntry::cost_for` accessor, not here.

use std::collections::HashMap;

use chrono::Utc;
use serde::Deserialize;

use crate::CatalogError;

use super::catalog::{ModelCatalogSnapshot, ModelEntry, ProviderEntry};

/// Top-level shape of models.dev `catalog.json`. We only need the providers
/// half today; the `models` top-level (provider-agnostic metadata: benchmarks,
/// weights, licenses) is intentionally ignored.
#[derive(Debug, Deserialize)]
struct CatalogJson {
    providers: HashMap<String, ProviderBlock>,
}

#[derive(Debug, Deserialize)]
struct ProviderBlock {
    #[serde(flatten)]
    entry: ProviderEntry,
    #[serde(default)]
    models: HashMap<String, ModelEntry>,
}

pub fn parse(json: &str) -> Result<ModelCatalogSnapshot, CatalogError> {
    let catalog: CatalogJson = serde_json::from_str(json)
        .map_err(|e| CatalogError::Internal(format!("metadata parse: {e}")))?;

    let mut entries = HashMap::new();
    let mut providers = HashMap::new();
    let mut protocol_defaults = HashMap::new();
    for (provider_id, block) in catalog.providers {
        let provider_default = block.entry.npm.as_deref().map(str::to_owned);
        providers.insert(provider_id.clone(), block.entry);
        for (model_id, model) in block.models {
            let model_default = model
                .provider
                .as_ref()
                .and_then(|adapter| adapter.npm.as_deref())
                .map(str::to_owned);
            if let Some(api) = model_default.or_else(|| provider_default.clone()) {
                protocol_defaults.insert(format!("{provider_id}/{model_id}"), api);
            }

            entries.insert(format!("{provider_id}/{model_id}"), model);
        }
    }

    let version = super::sources::digest(json.as_bytes())[..12].to_string();

    Ok(ModelCatalogSnapshot {
        providers,
        version,
        fetched_at: Utc::now(),
        entries,
        protocol_defaults,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
            "models": {},
            "providers": {
                "anthropic": {
                    "id": "anthropic",
                    "npm": "@ai-sdk/anthropic",
                    "models": {
                        "claude-opus-4-7": {
                            "id": "claude-opus-4-7",
                            "attachment": true,
                            "reasoning": true,
                            "tool_call": true,
                            "open_weights": false,
                            "structured_output": true,
                            "modalities": {"input": ["text", "image"], "output": ["text"]},
                            "limit": {"context": 1000000, "output": 128000},
                            "cost": {"input": 5, "output": 25, "cache_read": 0.5, "cache_write": 6.25}
                        },
                        "no-cost-stub": {
                            "id": "no-cost-stub",
                            "attachment": false,
                            "reasoning": false,
                            "tool_call": false,
                            "open_weights": true,
                            "modalities": {"input": ["text"], "output": ["text"]},
                            "limit": {"context": 8192, "output": 4096}
                        }
                    }
                },
                "openai": {
                    "id": "openai",
                    "npm": "@ai-sdk/openai-compatible",
                    "models": {
                        "gpt-4o": {
                            "id": "gpt-4o",
                            "attachment": true,
                            "reasoning": false,
                            "tool_call": true,
                            "open_weights": false,
                            "structured_output": true,
                            "modalities": {"input": ["text", "image"], "output": ["text"]},
                            "limit": {"context": 128000, "output": 16384},
                            "cost": {"input": 2.5, "output": 10},
                            "provider": {"npm": "@ai-sdk/openai"}
                        },
                        "gpt-4o-mini": {
                            "id": "gpt-4o-mini",
                            "attachment": false,
                            "reasoning": false,
                            "tool_call": true,
                            "open_weights": false,
                            "modalities": {"input": ["text"], "output": ["text"]},
                            "limit": {"context": 128000, "output": 16384},
                            "cost": {"input": 0.15, "output": 0.6}
                        },
                        "no-cost-openai": {
                            "id": "no-cost-openai",
                            "attachment": false,
                            "reasoning": false,
                            "tool_call": false,
                            "open_weights": false,
                            "modalities": {"input": ["text"], "output": ["text"]},
                            "limit": {"context": 8192, "output": 4096}
                        }
                    }
                }
            }
        }"#;

    #[test]
    fn parse_deserializes_nested_blocks() {
        let snapshot = parse(SAMPLE).expect("parse");
        let opus = snapshot
            .entries
            .get("anthropic/claude-opus-4-7")
            .expect("opus entry");
        let cost = opus.cost.as_ref().expect("cost");
        // Stored as published - USD per 1M tokens, no rescale at this layer.
        assert_eq!(cost.input, 5.0);
        assert_eq!(cost.output, 25.0);
        assert_eq!(cost.cache_read, Some(0.5));
        assert_eq!(cost.cache_write, Some(6.25));
        assert_eq!(opus.limit.context, 1_000_000);
        assert_eq!(opus.limit.output, 128_000);
    }

    #[test]
    fn parse_keys_entries_by_composite_provider_model() {
        let snapshot = parse(SAMPLE).expect("parse");
        assert!(snapshot.entries.contains_key("anthropic/claude-opus-4-7"));
        assert!(snapshot.entries.contains_key("openai/gpt-4o"));
        assert!(snapshot.entries.contains_key("anthropic/no-cost-stub"));
        assert!(!snapshot.entries["anthropic/no-cost-stub"].has_pricing());
    }

    #[test]
    fn cost_for_rescales_per_million_to_per_token() {
        let snapshot = parse(SAMPLE).expect("parse");
        let opus = snapshot.entries.get("anthropic/claude-opus-4-7").unwrap();
        // Convention: input_tokens is fresh. 1M * $5/M + 0.5M * $25/M = $17.5.
        let total = opus
            .cost_for(&crate::catalog::TokenUsage {
                input_tokens: 1_000_000,
                output_tokens: 500_000,
                cached_input_tokens: 0,
            })
            .expect("cost");
        assert!((total - 17.5).abs() < 1e-9);
    }

    #[test]
    fn max_input_tokens_subtracts_output_reservation() {
        let snapshot = parse(SAMPLE).expect("parse");
        let opus = snapshot.entries.get("anthropic/claude-opus-4-7").unwrap();
        // 1_000_000 context - 128_000 output = 872_000 input budget.
        assert_eq!(opus.max_input_tokens(), Some(872_000));
        assert_eq!(opus.max_output_tokens(), Some(128_000));
    }

    #[test]
    fn capability_helpers_derive_correctly() {
        let snapshot = parse(SAMPLE).expect("parse");
        let opus = snapshot.entries.get("anthropic/claude-opus-4-7").unwrap();
        assert!(opus.supports_function_calling());
        assert!(opus.supports_vision());
        assert!(opus.supports_prompt_caching());
        assert!(opus.supports_reasoning());
        assert!(opus.supports_response_schema());

        let gpt = snapshot.entries.get("openai/gpt-4o").unwrap();
        // No cache_read / cache_write in cost block -> no caching support.
        assert!(!gpt.supports_prompt_caching());
        // No reasoning flag -> no reasoning.
        assert!(!gpt.supports_reasoning());
    }
}
