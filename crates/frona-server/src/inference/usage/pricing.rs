//! Convert Rig token counts to the catalog's fresh-input pricing convention.

use crate::inference::{protocol::parameters::WireDialect, provider::ModelConfig};
use frona_model_catalog::{ModelCatalogSnapshot, catalog::TokenUsage};
use rig_core::completion::request::Usage;

pub(super) fn compute(
    snapshot: &ModelCatalogSnapshot,
    model: &ModelConfig,
    usage: &Usage,
) -> (Option<f64>, String) {
    let normalized = TokenUsage {
        input_tokens: if WireDialect::for_model(&model.provider) == WireDialect::Anthropic {
            usage.input_tokens
        } else {
            // Total-shaped prompt counts already include cache reads and writes.
            usage
                .input_tokens
                .saturating_sub(usage.cached_input_tokens)
                .saturating_sub(usage.cache_creation_input_tokens)
        },
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
    };
    (
        snapshot
            .lookup_prefix(&model.catalog_provider, &model.model_id)
            .and_then(|entry| entry.cost_for(&normalized)),
        snapshot.version.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use frona_model_catalog::catalog::{Cost, ModelEntry};
    use serde_json::json;

    fn fixture(provider: &str) -> (ModelCatalogSnapshot, ModelConfig) {
        let mut snapshot = ModelCatalogSnapshot::empty();
        snapshot.entries.insert(
            "catalog-account/fixture".into(),
            ModelEntry {
                cost: Some(Cost {
                    input: 3.0,
                    output: 10.0,
                    cache_read: Some(0.3),
                    cache_write: Some(3.75),
                }),
                ..Default::default()
            },
        );
        let model = ModelConfig {
            request_settings: Default::default(),
            catalog_provider: "catalog-account".into(),
            provider_handle: crate::core::Handle::try_new("work-account").unwrap(),
            model_id: "fixture".into(),
            provider: crate::core::config::ProviderModel::from_name(provider),
        };
        (snapshot, model)
    }

    #[test]
    fn equivalent_cache_usage_has_equal_cost_across_wire_dialects() {
        for (provider, input_tokens) in [
            ("openrouter", 1_000_000),
            ("openai", 1_000_000),
            ("anthropic", 200_000),
        ] {
            let (snapshot, mut model) = fixture(provider);
            // These deliberately disagree with both the wire dialect and catalog identity.
            for handle in ["work-account", "anthropic", "openrouter"] {
                model.provider_handle = crate::core::Handle::try_new(handle).unwrap();
                let mut usage = Usage {
                    input_tokens,
                    cached_input_tokens: 600_000,
                    cache_creation_input_tokens: 200_000,
                    ..Default::default()
                };
                assert!((compute(&snapshot, &model, &usage).0.unwrap() - 1.53).abs() < 1e-9);
                usage.output_tokens = 100_000;
                assert!((compute(&snapshot, &model, &usage).0.unwrap() - 2.53).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn total_shaped_counts_saturate_without_overflowing_cache_sum() {
        let (snapshot, model) = fixture("openrouter");
        for (input_tokens, reads, writes) in [
            (100, 200, 300),
            (300, 100, 300),
            (u64::MAX, u64::MAX, u64::MAX),
        ] {
            let usage = Usage {
                input_tokens,
                cached_input_tokens: reads,
                cache_creation_input_tokens: writes,
                ..Default::default()
            };
            let expected = (reads as f64 * 0.3 + writes as f64 * 3.75) / 1_000_000.0;
            let actual = compute(&snapshot, &model, &usage).0.unwrap();
            assert!((actual - expected).abs() <= expected * 1e-12);
        }
    }

    #[test]
    fn preserves_exact_catalog_rates_and_unknown_cost() {
        let (mut snapshot, model) = fixture("openrouter");
        let usage = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 600_000,
            cache_creation_input_tokens: 200_000,
            ..Default::default()
        };
        // A native-provider entry must never replace this connection's catalog pricing.
        snapshot.entries.insert(
            "openrouter/fixture".into(),
            ModelEntry {
                cost: Some(Cost {
                    input: 99.0,
                    output: 99.0,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let entry = snapshot.entries.get_mut("catalog-account/fixture").unwrap();
        entry.cost.as_mut().unwrap().cache_write = None;
        assert!((compute(&snapshot, &model, &usage).0.unwrap() - 1.38).abs() < 1e-9);
        snapshot
            .entries
            .get_mut("catalog-account/fixture")
            .unwrap()
            .cost = None;
        assert_eq!(compute(&snapshot, &model, &usage).0, None);
        snapshot.entries.remove("catalog-account/fixture");
        assert_eq!(compute(&snapshot, &model, &usage).0, None);
    }

    #[test]
    fn pricing_uses_family_fallback_only_after_exact_catalog_matches() {
        let (mut snapshot, mut model) = fixture("openrouter");
        snapshot.entries.clear();
        snapshot.version = "a1b2c3d4e5f6".into();
        for (key, input) in [
            ("openrouter/anthropic/claude-sonnet-4-20250514", 9.0),
            ("anthropic/claude-sonnet-4", 3.0),
            ("anthropic/claude", 99.0),
            ("bare-model", 5.0),
        ] {
            snapshot.entries.insert(
                key.into(),
                ModelEntry {
                    cost: Some(Cost {
                        input,
                        output: 10.0,
                        cache_read: Some(0.3),
                        cache_write: Some(3.75),
                    }),
                    ..Default::default()
                },
            );
        }
        for key in [
            "openrouter/anthropic/claude-sonnet-4-unpriced",
            "anthropic/claude-sonnet-4-unpriced",
        ] {
            snapshot.entries.insert(key.into(), ModelEntry::default());
        }
        // Keep cache reads/writes in the calculation to protect their rates when
        // selecting either an exact entry or a family estimate.
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cached_input_tokens: 600_000,
            cache_creation_input_tokens: 200_000,
            ..Default::default()
        };
        for (provider, model_id, expected) in [
            (
                "openrouter",
                "anthropic/claude-sonnet-4-20250514",
                Some(3.73),
            ),
            ("openrouter", "anthropic/claude-sonnet-4", Some(2.53)),
            (
                "openrouter",
                "anthropic/claude-sonnet-4-20250515",
                Some(2.53),
            ),
            ("anthropic", "claude-sonnet-4-latest", Some(2.53)),
            ("openrouter", "bare-model", Some(2.93)),
            ("openrouter", "anthropic/claude-sonnet-4-unpriced", None),
            ("anthropic", "claude-sonnet-4-unpriced", None),
            ("openrouter", "anthropic/unrelated-model", None),
            ("unrelated-provider", "claude-sonnet-4-latest", None),
        ] {
            model.catalog_provider = provider.into();
            model.model_id = model_id.into();
            for handle in ["work-account", "anthropic", "openrouter"] {
                model.provider_handle = crate::core::Handle::try_new(handle).unwrap();
                let (cost, version) = compute(&snapshot, &model, &usage);
                match expected {
                    Some(expected) => assert!(
                        (cost.unwrap() - expected).abs() < 1e-9,
                        "{provider}/{model_id} via {handle}: {cost:?}"
                    ),
                    None => assert_eq!(cost, None, "{provider}/{model_id} via {handle}"),
                }
                assert_eq!(version, "a1b2c3d4e5f6");
            }
        }
    }

    #[test]
    fn openrouter_wire_usage_preserves_cache_breakdown_or_leaves_it_zero() {
        // Rig uses this same usage type for completion responses and terminal SSE chunks.
        for details in [
            None,
            Some(json!({})),
            Some(json!({"cached_tokens": 600_000, "cache_write_tokens": 200_000})),
        ] {
            let mut wire = json!({"prompt_tokens": 1_000_000, "completion_tokens": 100_000, "total_tokens": 1_100_000});
            if let Some(details) = &details {
                wire["prompt_tokens_details"] = details.clone();
            }
            let usage: rig_core::providers::openrouter::Usage =
                serde_json::from_value(wire).unwrap();
            let usage = Usage::from(usage);
            let has_breakdown = details
                .as_ref()
                .is_some_and(|d| d.get("cache_write_tokens").is_some());
            assert_eq!(usage.input_tokens, 1_000_000);
            assert_eq!(
                usage.cached_input_tokens,
                if has_breakdown { 600_000 } else { 0 }
            );
            assert_eq!(
                usage.cache_creation_input_tokens,
                if has_breakdown { 200_000 } else { 0 }
            );
            let (snapshot, model) = fixture("openrouter");
            let expected = if has_breakdown { 2.53 } else { 4.0 };
            assert!((compute(&snapshot, &model, &usage).0.unwrap() - expected).abs() < 1e-9);
        }
    }

    #[test]
    fn anthropic_completion_and_stream_usage_preserve_disjoint_cache_counts() {
        for cache_counts in [None, Some((0, 0)), Some((600_000, 200_000))] {
            let mut wire = json!({"input_tokens": 200_000, "output_tokens": 100_000});
            if let Some((reads, writes)) = cache_counts {
                wire["cache_read_input_tokens"] = json!(reads);
                wire["cache_creation_input_tokens"] = json!(writes);
            }
            let completion: rig_core::providers::anthropic::completion::Usage =
                serde_json::from_value(wire.clone()).unwrap();
            let streaming: rig_core::providers::anthropic::streaming::PartialUsage =
                serde_json::from_value(wire).unwrap();
            for usage in [Usage::from(completion), Usage::from(streaming)] {
                let (reads, writes) = cache_counts.unwrap_or_default();
                assert_eq!(usage.input_tokens, 200_000);
                assert_eq!(usage.cached_input_tokens, reads);
                assert_eq!(usage.cache_creation_input_tokens, writes);
                let (snapshot, model) = fixture("anthropic");
                let expected = if writes > 0 { 2.53 } else { 1.6 };
                assert!((compute(&snapshot, &model, &usage).0.unwrap() - expected).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn normalizes_rig_usage_without_changing_catalog_prices() {
        for (provider, input, expected) in [
            ("openai", 1_000_000, 1.5),
            ("anthropic", 600_000, 3.2),
            ("anthropic", 200_000, 1.2),
        ] {
            let mut snapshot = ModelCatalogSnapshot::empty();
            snapshot.entries.insert(
                format!("{provider}/fixture"),
                ModelEntry {
                    cost: Some(Cost {
                        input: if provider == "openai" { 2.5 } else { 5.0 },
                        output: 25.0,
                        cache_read: if provider == "anthropic" {
                            Some(0.5)
                        } else {
                            None
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
            let mut model = ModelConfig {
                request_settings: Default::default(),
                catalog_provider: provider.into(),
                provider_handle: crate::core::Handle::try_new(provider).unwrap(),
                model_id: "fixture".into(),
                provider: crate::core::config::ProviderModel::from_name(provider),
            };
            let usage = Usage {
                input_tokens: input,
                cached_input_tokens: 400_000,
                ..Default::default()
            };
            for handle in [provider, "work-account", "other-account"] {
                model.provider_handle = crate::core::Handle::try_new(handle).unwrap();
                let (cost, version) = compute(&snapshot, &model, &usage);
                assert!((cost.unwrap() - expected).abs() < 1e-9);
                assert_eq!(version, snapshot.version);
            }
            model.model_id = "unknown".into();
            assert!(compute(&snapshot, &model, &usage).0.is_none());
            snapshot
                .entries
                .insert(format!("{provider}/unknown"), ModelEntry::default());
            assert!(compute(&snapshot, &model, &usage).0.is_none());
        }
    }
}
