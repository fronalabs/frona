//! Convert Rig token counts to the catalog's fresh-input pricing convention.

use crate::inference::provider::ModelConfig;
use frona_model_catalog::{ModelCatalogSnapshot, catalog::TokenUsage};
use rig_core::completion::request::Usage;

pub(super) fn compute(
    snapshot: &ModelCatalogSnapshot,
    model: &ModelConfig,
    usage: &Usage,
) -> (Option<f64>, String) {
    let normalized = TokenUsage {
        input_tokens: if crate::inference::protocol::parameters::WireDialect::for_model(
            &model.provider,
        ) == crate::inference::protocol::parameters::WireDialect::Anthropic
        {
            usage.input_tokens
        } else {
            usage.input_tokens.saturating_sub(usage.cached_input_tokens)
        },
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
    };
    (
        snapshot
            .lookup_for_provider(&model.catalog_provider, &model.model_id)
            .and_then(|entry| entry.cost_for(&normalized)),
        snapshot.version.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use frona_model_catalog::catalog::{Cost, ModelEntry};

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
