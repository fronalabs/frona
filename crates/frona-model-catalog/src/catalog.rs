//! Mirrors the models.dev catalog shape directly so the loader is a one-step
//! deserialize. All `Cost` fields are USD per **1M tokens** as published
//! upstream; `ModelEntry::cost_for` handles the per-token rescale internally.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};

use serde::Deserialize;

/// Required-vs-optional matches the upstream Zod schema in
/// `github.com/anomalyco/models.dev` `packages/core/src/schema.ts`.
/// Parsing fails-loud if upstream drops a required field.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub provider: Option<ModelRoute>,
    #[serde(default)]
    pub reasoning_options: Vec<serde_json::Value>,
    #[serde(flatten)]
    pub authoring: HashMap<String, serde_json::Value>,
    pub attachment: bool,
    pub reasoning: bool,
    pub tool_call: bool,
    pub open_weights: bool,
    pub limit: Limit,
    pub modalities: Modalities,

    /// Optional upstream - open-weights models without hosted pricing.
    #[serde(default)]
    pub cost: Option<Cost>,

    #[serde(default)]
    pub structured_output: bool,
    #[serde(default)]
    pub temperature: bool,

    /// `"alpha" | "beta" | "deprecated"`; `None` means generally available.
    #[serde(default)]
    pub status: Option<String>,

    /// YYYY-MM-DD or YYYY-MM when published.
    #[serde(default)]
    pub knowledge: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ModelRoute {
    pub npm: Option<String>,
    pub api: Option<String>,
    #[serde(flatten)]
    pub attributes: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ProviderEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub env: Vec<String>,
    pub npm: Option<String>,
    pub api: Option<String>,
    pub doc: Option<String>,
    #[serde(flatten)]
    pub authoring: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    /// Anthropic-only; other providers don't publish a cache-write rate.
    #[serde(default)]
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Limit {
    pub context: u64,
    pub output: u64,
    #[serde(default)]
    pub input: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Modalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

const PER_MILLION_TO_PER_TOKEN: f64 = 1.0 / 1_000_000.0;

/// Input tokens are fresh tokens only; cached tokens are additive.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

impl ModelEntry {
    /// Calculate USD from published per-million rates and normalized tokens.
    pub fn cost_for(&self, u: &TokenUsage) -> Option<f64> {
        let cost = self.cost.as_ref()?;
        let cache_read = cost.cache_read.unwrap_or(0.0);
        let total = (u.input_tokens as f64) * cost.input
            + (u.output_tokens as f64) * cost.output
            + (u.cached_input_tokens as f64) * cache_read;
        Some(total * PER_MILLION_TO_PER_TOKEN)
    }

    pub fn max_input_tokens(&self) -> Option<u64> {
        if self.limit.context == 0 {
            return None;
        }
        if let Some(input) = self.limit.input {
            return Some(input);
        }
        Some(self.limit.context.saturating_sub(self.limit.output))
    }

    /// Zero is the `Default::default()` sentinel - real models always
    /// publish a positive output limit, so we map zero to `None`.
    pub fn max_output_tokens(&self) -> Option<u64> {
        if self.limit.output == 0 {
            None
        } else {
            Some(self.limit.output)
        }
    }

    pub fn supports_function_calling(&self) -> bool {
        self.tool_call
    }

    pub fn supports_vision(&self) -> bool {
        self.attachment || self.modalities.input.iter().any(|s| s == "image")
    }

    pub fn supports_prompt_caching(&self) -> bool {
        self.cost
            .as_ref()
            .is_some_and(|c| c.cache_read.is_some() || c.cache_write.is_some())
    }

    pub fn supports_reasoning(&self) -> bool {
        self.reasoning
    }

    pub fn supports_response_schema(&self) -> bool {
        self.structured_output
    }

    /// Strict schema guarantees `input` / `output` are present whenever
    /// `cost` is, so the test reduces to `cost.is_some()`.
    pub fn has_pricing(&self) -> bool {
        self.cost.is_some()
    }
}

#[derive(Debug)]
pub struct ModelCatalogSnapshot {
    pub providers: HashMap<String, ProviderEntry>,
    /// SHA-256 prefix (first 12 chars) of the source JSON bytes. Every row
    /// written under this version shares it.
    pub version: String,
    pub fetched_at: DateTime<Utc>,
    pub entries: HashMap<String, ModelEntry>,
    /// Upstream npm labels, not executable protocol choices. Callers interpret them.
    pub protocol_defaults: HashMap<String, String>,
}

impl ModelCatalogSnapshot {
    pub fn effective_route(&self, provider: &str, model: &str) -> Option<ModelRoute> {
        let record = self.providers.get(provider)?;
        let override_ = self
            .entries
            .get(&format!("{provider}/{model}"))?
            .provider
            .as_ref();
        Some(ModelRoute {
            npm: override_
                .and_then(|route| route.npm.clone())
                .or_else(|| record.npm.clone()),
            api: override_
                .and_then(|route| route.api.clone())
                .or_else(|| record.api.clone()),
            attributes: override_
                .map(|route| route.attributes.clone())
                .unwrap_or_default(),
        })
    }

    /// No external source is loaded. Callers own any application fallback policy.
    pub fn empty() -> Self {
        Self {
            version: "empty".to_string(),
            fetched_at: Utc::now(),
            entries: HashMap::new(),
            providers: HashMap::new(),
            protocol_defaults: HashMap::new(),
        }
    }

    /// Look up the exact provider/model key, then a bare model key supplied by
    /// a caller's fallback snapshot. Configured connection handles are not catalog IDs.
    pub fn lookup_for_provider(&self, provider: &str, model_id: &str) -> Option<&ModelEntry> {
        let composite = format!("{provider}/{model_id}");
        if let Some(p) = self.entries.get(&composite) {
            return Some(p);
        }
        self.entries.get(model_id)
    }

    /// Like `lookup` but falls back to a longest-prefix walk so dated-suffix
    /// ids returned by provider APIs (e.g. `claude-opus-4-7-20250708`) still
    /// resolve to their family entry. When the model_id itself contains a
    /// vendor prefix (`qwen/qwen3.6-flash` from OpenRouter, Together AI etc.),
    /// the walk re-scopes to that vendor so provider-of-providers IDs resolve
    /// against the underlying vendor's catalog section.
    pub fn lookup_prefix(&self, provider: &str, model_id: &str) -> Option<&ModelEntry> {
        if let Some(entry) = self.lookup_for_provider(provider, model_id) {
            return Some(entry);
        }
        if let Some((vendor, rest)) = model_id.split_once('/') {
            if let Some(entry) = self.lookup_for_provider(vendor, rest) {
                return Some(entry);
            }
            return self.prefix_walk(vendor, rest);
        }

        self.prefix_walk(provider, model_id)
    }

    pub fn protocol_hint(&self, provider: &str, model_id: &str) -> Option<&str> {
        self.protocol_defaults
            .get(&format!("{provider}/{model_id}"))
            .map(String::as_str)
    }

    fn prefix_walk(&self, provider: &str, model_id: &str) -> Option<&ModelEntry> {
        let provider_prefix = format!("{provider}/");
        let mut best: Option<(usize, &str)> = None;
        for key in self.entries.keys() {
            let normalized = if let Some(stripped) = key.strip_prefix(&provider_prefix) {
                stripped
            } else if !key.contains('/') {
                key.as_str()
            } else {
                continue;
            };
            if !normalized.is_empty()
                && model_id.starts_with(normalized)
                && best.is_none_or(|(len, _)| normalized.len() > len)
            {
                best = Some((normalized.len(), key.as_str()));
            }
        }
        best.and_then(|(_, key)| self.entries.get(key))
    }
}

/// Hot-swappable wrapper around `Arc<ModelCatalogSnapshot>`. Readers call
/// `current()` for a cheap `Arc<...>`; the scheduler `swap()`s atomically.
/// Internal `Arc`s make clones cheap - `AppState` holds a bare
/// `ModelCatalogStore`, not an `Arc<ModelCatalogStore>`.
#[derive(Clone)]
pub struct ModelCatalogStore {
    inner: Arc<ArcSwap<ModelCatalogSnapshot>>,
    last_refresh_unix: Arc<AtomicI64>,
}

impl ModelCatalogStore {
    pub fn new(initial: ModelCatalogSnapshot) -> Self {
        let fetched_at = initial.fetched_at.timestamp();
        Self {
            inner: Arc::new(ArcSwap::new(Arc::new(initial))),
            last_refresh_unix: Arc::new(AtomicI64::new(fetched_at)),
        }
    }

    pub fn current(&self) -> Arc<ModelCatalogSnapshot> {
        self.inner.load_full()
    }

    pub fn swap(&self, next: ModelCatalogSnapshot) {
        let fetched_at = next.fetched_at.timestamp();
        self.inner.store(Arc::new(next));
        self.last_refresh_unix.store(fetched_at, Ordering::Relaxed);
    }

    pub fn seconds_since_refresh(&self) -> i64 {
        (Utc::now().timestamp() - self.last_refresh_unix.load(Ordering::Relaxed)).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_prefix_matches_dated_suffix_against_bare_key() {
        let mut snap = ModelCatalogSnapshot::empty();
        snap.entries.insert(
            "claude-opus-4-7".into(),
            ModelEntry {
                limit: Limit {
                    context: 200_000,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let entry = snap
            .lookup_prefix("anthropic", "claude-opus-4-7-20251210")
            .expect("dated suffix should fall back to bare prefix");
        assert_eq!(entry.limit.context, 200_000);
    }

    #[test]
    fn lookup_prefix_prefers_longest_match() {
        let mut entries = HashMap::new();
        entries.insert(
            "openai/gpt-4o".into(),
            ModelEntry {
                limit: Limit {
                    context: 128_000,
                    output: 16_384,
                    input: None,
                },
                ..Default::default()
            },
        );
        entries.insert(
            "openai/gpt-4o-mini".into(),
            ModelEntry {
                limit: Limit {
                    context: 128_000,
                    output: 32_768,
                    input: None,
                },
                ..Default::default()
            },
        );
        let snap = ModelCatalogSnapshot {
            version: "test".into(),
            fetched_at: Utc::now(),
            entries,
            providers: HashMap::new(),
            protocol_defaults: HashMap::new(),
        };
        let entry = snap
            .lookup_prefix("openai", "gpt-4o-mini-2024-07-18")
            .expect("longest prefix should win");
        assert_eq!(entry.limit.output, 32_768);
    }

    #[test]
    fn lookup_prefix_resolves_openrouter_vendor_namespace() {
        let mut entries = HashMap::new();
        entries.insert(
            "qwen/qwen3-coder".into(),
            ModelEntry {
                limit: Limit {
                    context: 256_000,
                    output: 65_536,
                    input: None,
                },
                ..Default::default()
            },
        );
        let snap = ModelCatalogSnapshot {
            version: "test".into(),
            fetched_at: Utc::now(),
            entries,
            providers: HashMap::new(),
            protocol_defaults: HashMap::new(),
        };
        let entry = snap
            .lookup_prefix("openrouter", "qwen/qwen3-coder-plus")
            .expect("vendor-namespaced openrouter id should resolve");
        assert_eq!(entry.limit.context, 256_000);
    }

    #[test]
    fn lookup_prefix_does_not_cross_providers() {
        let mut entries = HashMap::new();
        entries.insert(
            "openai/gpt-4o".into(),
            ModelEntry {
                limit: Limit {
                    context: 128_000,
                    output: 16_384,
                    input: None,
                },
                ..Default::default()
            },
        );
        let snap = ModelCatalogSnapshot {
            version: "test".into(),
            fetched_at: Utc::now(),
            entries,
            providers: HashMap::new(),
            protocol_defaults: HashMap::new(),
        };
        assert!(snap.lookup_prefix("anthropic", "gpt-4o-mini").is_none());
    }
}
