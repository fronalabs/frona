//! Conservative server fallback metadata used when no external catalog is available.

use chrono::Utc;
use frona_model_catalog::catalog::{Limit, ModelCatalogSnapshot, ModelEntry};
use std::collections::HashMap;

/// Hardcoded fallback for the no-cache first-boot path. Carries context
/// windows + capability flags only - pricing is `None` until the
/// scheduler's first refresh fills in live models.dev data.
pub fn defaults() -> ModelCatalogSnapshot {
    let mut entries = HashMap::new();

    let claude = ModelEntry {
        limit: Limit {
            context: 200_000,
            output: 32_000,
            input: None,
        },
        attachment: true,
        reasoning: true,
        tool_call: true,
        structured_output: true,
        ..Default::default()
    };
    for id in [
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-4-6",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
        "claude-fable-5",
    ] {
        entries.insert(id.into(), claude.clone());
    }

    let gpt_4x = ModelEntry {
        limit: Limit {
            context: 128_000,
            output: 16_384,
            input: None,
        },
        attachment: true,
        tool_call: true,
        structured_output: true,
        ..Default::default()
    };
    for id in ["gpt-4o", "gpt-4.1", "gpt-4.5"] {
        entries.insert(id.into(), gpt_4x.clone());
    }

    let o_series = ModelEntry {
        limit: Limit {
            context: 200_000,
            output: 65_536,
            input: None,
        },
        tool_call: true,
        reasoning: true,
        structured_output: true,
        ..Default::default()
    };
    for id in ["o1", "o3", "o4"] {
        entries.insert(id.into(), o_series.clone());
    }

    let gemini_long = ModelEntry {
        limit: Limit {
            context: 1_000_000,
            output: 8_192,
            input: None,
        },
        attachment: true,
        tool_call: true,
        structured_output: true,
        ..Default::default()
    };
    for id in [
        "gemini-2.0-flash",
        "gemini-2.5-pro",
        "gemini-2.5-flash",
        "gemini-1.5-pro",
    ] {
        entries.insert(id.into(), gemini_long.clone());
    }

    entries.insert(
        "deepseek-chat".into(),
        ModelEntry {
            limit: Limit {
                context: 64_000,
                output: 8_192,
                input: None,
            },
            tool_call: true,
            structured_output: true,
            ..Default::default()
        },
    );
    entries.insert(
        "deepseek-reasoner".into(),
        ModelEntry {
            limit: Limit {
                context: 64_000,
                output: 8_192,
                input: None,
            },
            tool_call: true,
            reasoning: true,
            ..Default::default()
        },
    );
    let dsv4 = ModelEntry {
        limit: Limit {
            context: 1_000_000,
            output: 8_192,
            input: None,
        },
        tool_call: true,
        structured_output: true,
        ..Default::default()
    };
    entries.insert("deepseek-v4-pro".into(), dsv4.clone());
    entries.insert("deepseek-v4-flash".into(), dsv4);

    for id in ["llama-3.3-70b-versatile", "llama-3.1-70b", "llama-3.1-405b"] {
        entries.insert(
            id.into(),
            ModelEntry {
                limit: Limit {
                    context: 128_000,
                    output: 8_192,
                    input: None,
                },
                tool_call: true,
                ..Default::default()
            },
        );
    }

    entries.insert(
        "grok-2-latest".into(),
        ModelEntry {
            limit: Limit {
                context: 131_072,
                output: 8_192,
                input: None,
            },
            tool_call: true,
            ..Default::default()
        },
    );

    entries.insert(
        "mistral-large-latest".into(),
        ModelEntry {
            limit: Limit {
                context: 128_000,
                output: 8_192,
                input: None,
            },
            tool_call: true,
            structured_output: true,
            ..Default::default()
        },
    );

    entries.insert(
        "command-r-plus".into(),
        ModelEntry {
            limit: Limit {
                context: 128_000,
                output: 4_096,
                input: None,
            },
            tool_call: true,
            ..Default::default()
        },
    );

    entries.insert(
        "qwen3-vl:32b".into(),
        ModelEntry {
            limit: Limit {
                context: 128_000,
                output: 8_192,
                input: None,
            },
            attachment: true,
            tool_call: true,
            ..Default::default()
        },
    );

    ModelCatalogSnapshot {
        version: "defaults".to_string(),
        fetched_at: Utc::now(),
        entries,
        providers: HashMap::new(),
        protocol_defaults: HashMap::new(),
    }
}
