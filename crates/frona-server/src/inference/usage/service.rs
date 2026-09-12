//! Single recording funnel: every successful provider call goes through
//! `record()`, which persists the row, emits Prometheus metrics, and
//! dispatches the SSE event.

use chrono::Utc;
use metrics::{counter, gauge, histogram};
use rig_core::completion::request::Usage;

use crate::chat::broadcast::BroadcastService;
use crate::core::repository::{Repository, new_id};
use crate::db::repo::generic::SurrealRepo;
use crate::inference::provider::ModelConfig;
use crate::inference::usage::UsageContext;
use frona_model_catalog::ModelCatalogStore;

use super::models::InferenceUsage;

// Metric names - `_total` suffix on counters per Prometheus convention; no
// `_histogram` suffix on histograms (auto-generated `_bucket`/`_count`/`_sum`).
pub const INFERENCE_INPUT_TOKENS_TOTAL: &str = "frona_inference_input_tokens_total";
pub const INFERENCE_CACHED_INPUT_TOKENS_TOTAL: &str = "frona_inference_cached_input_tokens_total";
pub const INFERENCE_OUTPUT_TOKENS_TOTAL: &str = "frona_inference_output_tokens_total";
pub const INFERENCE_COST_USD_TOTAL: &str = "frona_inference_cost_usd_total";
pub const INFERENCE_CALLS_TOTAL: &str = "frona_inference_calls_total";
pub const INFERENCE_DURATION_MS: &str = "frona_inference_duration_ms";
pub const INFERENCE_TTFT_MS: &str = "frona_inference_ttft_ms";
pub const INFERENCE_OUTPUT_TOKENS_PER_SECOND: &str = "frona_inference_output_tokens_per_second";
pub const INFERENCE_RETRY_OVERHEAD_MS: &str = "frona_inference_retry_overhead_ms";
pub const INFERENCE_RETRIES_TOTAL: &str = "frona_inference_retries_total";
pub const MODEL_METADATA_LOOKUP_MISSES_TOTAL: &str = "frona_model_metadata_lookup_misses_total";
pub const MODEL_METADATA_ENTRIES: &str = "frona_model_metadata_entries";
pub const MODEL_METADATA_REFRESH_AGE_SECONDS: &str =
    "frona_model_metadata_refresh_seconds_since_last";

fn record_catalog_size(snapshot: &frona_model_catalog::ModelCatalogSnapshot) {
    gauge!(MODEL_METADATA_ENTRIES, "version" => snapshot.version.clone())
        .set(snapshot.entries.len() as f64);
}

/// Latency reading captured by the retry layer for a single recorded call.
/// `retry_overhead_ms` + `retry_count` describe retries within the recorded
/// model only; cross-model fallback is captured by `fallback_index`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencyMetrics {
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub retry_overhead_ms: u64,
    pub retry_count: u32,
}

#[derive(Clone)]
pub struct UsageService {
    /// Shared with `CatalogSources.models`. `ModelCatalogStore` is internally
    /// `Arc`-wrapped, so all clones share the same underlying ArcSwap - the
    /// scheduler can `swap()` from outside this service and we observe it.
    catalog: ModelCatalogStore,
    repo: SurrealRepo<InferenceUsage>,
    broadcast: BroadcastService,
}

impl UsageService {
    pub fn new(
        catalog: ModelCatalogStore,
        repo: SurrealRepo<InferenceUsage>,
        broadcast: BroadcastService,
    ) -> Self {
        record_catalog_size(&catalog.current());
        Self {
            catalog,
            repo,
            broadcast,
        }
    }

    /// The only way to record an inference call.
    pub async fn record(
        &self,
        usage_ctx: &UsageContext,
        model_ref: &ModelConfig,
        usage: &Usage,
        fallback_index: u8,
        latency: LatencyMetrics,
    ) {
        let snapshot = self.catalog.current();
        record_catalog_size(&snapshot);
        let (cost_usd, pricing_version) = super::pricing::compute(&snapshot, model_ref, usage);
        if cost_usd.is_none() {
            counter!(MODEL_METADATA_LOOKUP_MISSES_TOTAL, "model_ref" => model_ref.as_str())
                .increment(1);
        }
        let row = build_row(
            usage_ctx,
            model_ref,
            usage,
            fallback_index,
            latency,
            cost_usd,
            pricing_version,
        );

        // Persistence failure logs but never propagates - observability never
        // blocks the user's reply.
        if let Err(e) = self.repo.create(&row).await {
            tracing::warn!(
                error = %e,
                "inference_usage persist failed; metrics + event still emitted"
            );
        }
        emit_metrics(&row);
        self.dispatch_event(&row);
    }

    /// Called periodically (or on scrape) to keep the refresh-age gauge fresh.
    pub fn touch_refresh_age_gauge(&self) {
        gauge!(MODEL_METADATA_REFRESH_AGE_SECONDS).set(self.catalog.seconds_since_refresh() as f64);
    }

    fn dispatch_event(&self, row: &InferenceUsage) {
        let Some(chat_id) = row.chat_id.as_ref() else {
            return;
        };
        self.broadcast
            .broadcast_usage_recorded(crate::chat::broadcast::UsageRecorded {
                chat_id: chat_id.clone(),
                user_id: row.user_id.clone(),
                agent_id: row.agent_id.clone(),
                message_id: row.message_id.clone(),
                kind_tag: row.kind_tag.clone(),
                model_ref: row.model_ref.clone(),
                input_tokens: row.input_tokens,
                cached_input_tokens: row.cached_input_tokens,
                output_tokens: row.output_tokens,
                cost_usd: row.cost_usd,
                duration_ms: row.duration_ms,
                ttft_ms: row.ttft_ms,
                output_tokens_per_second: row.output_tokens_per_second,
                retry_overhead_ms: row.retry_overhead_ms,
                retry_count: row.retry_count,
                fallback_index: row.fallback_index,
            });
    }
}

fn emit_metrics(row: &InferenceUsage) {
    let user_id = row.user_id.clone();
    let agent_id = row.agent_id.clone().unwrap_or_default();
    let fb = row.fallback_index.to_string();
    let provider = row.provider.clone();
    let model_id = row.model_id.clone();
    let model_group = row.model_group.clone();
    let kind_tag = row.kind_tag.clone();

    let token_labels = [
        ("provider", provider.clone()),
        ("model_id", model_id.clone()),
        ("model_group", model_group.clone()),
        ("user_id", user_id.clone()),
        ("agent_id", agent_id.clone()),
        ("kind_tag", kind_tag.clone()),
        ("fallback_index", fb.clone()),
    ];
    let cost_labels = [
        ("provider", provider.clone()),
        ("model_id", model_id.clone()),
        ("model_group", model_group.clone()),
        ("user_id", user_id),
        ("agent_id", agent_id),
        ("kind_tag", kind_tag.clone()),
    ];
    let latency_labels = [
        ("provider", provider),
        ("model_id", model_id),
        ("kind_tag", kind_tag),
    ];

    counter!(INFERENCE_INPUT_TOKENS_TOTAL, &token_labels).increment(row.input_tokens);
    counter!(INFERENCE_CACHED_INPUT_TOKENS_TOTAL, &token_labels).increment(row.cached_input_tokens);
    counter!(INFERENCE_OUTPUT_TOKENS_TOTAL, &token_labels).increment(row.output_tokens);
    counter!(INFERENCE_CALLS_TOTAL, &token_labels).increment(1);
    counter!(INFERENCE_RETRIES_TOTAL, &token_labels).increment(row.retry_count as u64);
    if let Some(cost) = row.cost_usd {
        // The `metrics` 0.24 counter API only accepts u64. Track cost in
        // micro-dollars (cost * 1_000_000) so we get 6-decimal precision
        // through Prometheus while preserving the integer-counter semantics.
        // Grafana queries multiply by 1e-6 to display dollars.
        let micros = (cost * 1_000_000.0).max(0.0) as u64;
        counter!(INFERENCE_COST_USD_TOTAL, &cost_labels).increment(micros);
    }
    histogram!(INFERENCE_DURATION_MS, &latency_labels).record(row.duration_ms as f64);
    if let Some(ttft) = row.ttft_ms {
        histogram!(INFERENCE_TTFT_MS, &latency_labels).record(ttft as f64);
    }
    if let Some(tps) = row.output_tokens_per_second {
        histogram!(INFERENCE_OUTPUT_TOKENS_PER_SECOND, &latency_labels).record(tps);
    }
    if row.retry_overhead_ms > 0 {
        histogram!(INFERENCE_RETRY_OVERHEAD_MS, &latency_labels)
            .record(row.retry_overhead_ms as f64);
    }
}

fn build_row(
    usage_ctx: &UsageContext,
    model_ref: &ModelConfig,
    usage: &Usage,
    fallback_index: u8,
    latency: LatencyMetrics,
    cost_usd: Option<f64>,
    pricing_version: String,
) -> InferenceUsage {
    let kind = &usage_ctx.kind;
    let output_tokens_per_second = compute_output_tps(usage.output_tokens, latency);
    InferenceUsage {
        id: new_id(),
        user_id: usage_ctx.user_id.clone(),
        agent_id: kind.agent_id().map(str::to_owned),
        chat_id: kind.chat_id().map(str::to_owned),
        space_id: kind.space_id().map(str::to_owned),
        message_id: kind.message_id().map(str::to_owned),
        turn_index: kind.turn_index(),
        kind_tag: kind.tag().to_owned(),
        model_group: usage_ctx.model_group.clone(),
        provider: model_ref.provider_name().to_string(),
        model_id: model_ref.model_id.clone(),
        model_ref: model_ref.as_str(),
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        fallback_index,
        duration_ms: latency.duration_ms,
        ttft_ms: latency.ttft_ms,
        output_tokens_per_second,
        retry_overhead_ms: latency.retry_overhead_ms,
        retry_count: latency.retry_count,
        cost_usd,
        pricing_version,
        created_at: Utc::now(),
    }
}

/// `output_tokens / generation_seconds` where generation_seconds is the wall
/// time after the first token arrived. Returns `None` if there's no usable
/// signal - no output tokens, no TTFT (non-streaming path), or duration shorter
/// than TTFT (clock skew between captures).
fn compute_output_tps(output_tokens: u64, latency: LatencyMetrics) -> Option<f64> {
    if output_tokens == 0 {
        return None;
    }
    let ttft = latency.ttft_ms?;
    let gen_ms = latency.duration_ms.checked_sub(ttft)?;
    if gen_ms == 0 {
        return None;
    }
    Some(output_tokens as f64 * 1000.0 / gen_ms as f64)
}
