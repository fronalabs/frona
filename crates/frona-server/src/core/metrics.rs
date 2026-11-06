use std::sync::OnceLock;
use std::time::Duration;

use metrics::{counter, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use rig_core::completion::request::Usage;

pub const HTTP_REQUESTS_TOTAL: &str = "frona_http_requests_total";
pub const HTTP_REQUEST_DURATION_SECONDS: &str = "frona_http_request_duration_seconds";
pub const INFERENCE_REQUESTS_TOTAL: &str = "frona_inference_requests_total";
pub const INFERENCE_REQUEST_DURATION_SECONDS: &str = "frona_inference_request_duration_seconds";
pub const INFERENCE_INPUT_TOKENS_TOTAL: &str = "frona_inference_input_tokens_total";
pub const INFERENCE_OUTPUT_TOKENS_TOTAL: &str = "frona_inference_output_tokens_total";
pub const INFERENCE_CACHED_INPUT_TOKENS_TOTAL: &str = "frona_inference_cached_input_tokens_total";
pub const TOOL_CALLS_TOTAL: &str = "frona_tool_calls_total";
pub const TOOL_CALL_DURATION_SECONDS: &str = "frona_tool_call_duration_seconds";
pub const POLICY_EVALUATIONS_TOTAL: &str = "frona_policy_evaluations_total";
pub const POLICY_EVALUATION_DURATION_SECONDS: &str = "frona_policy_evaluation_duration_seconds";

static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

pub fn setup_metrics_recorder() -> PrometheusHandle {
    METRICS_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("Failed to install Prometheus metrics recorder")
        })
        .clone()
}

#[derive(Debug, Clone, Default)]
pub struct InferenceMetricsContext {
    pub user_id: String,
    pub agent_id: String,
    pub model_group: String,
}

pub fn record_inference_request(
    ctx: &InferenceMetricsContext,
    model: &str,
    provider: &str,
    duration: Duration,
    usage: Option<&Usage>,
    outcome: &str,
) {
    let labels = [
        ("model", model.to_string()),
        ("provider", provider.to_string()),
        ("model_group", ctx.model_group.clone()),
        ("user_id", ctx.user_id.clone()),
        ("agent_id", ctx.agent_id.clone()),
        ("outcome", outcome.to_string()),
    ];

    counter!(INFERENCE_REQUESTS_TOTAL, &labels).increment(1);
    histogram!(INFERENCE_REQUEST_DURATION_SECONDS, &labels).record(duration.as_secs_f64());

    if let Some(usage) = usage {
        let token_labels = [
            ("model", model.to_string()),
            ("provider", provider.to_string()),
            ("model_group", ctx.model_group.clone()),
            ("user_id", ctx.user_id.clone()),
            ("agent_id", ctx.agent_id.clone()),
        ];
        counter!(INFERENCE_INPUT_TOKENS_TOTAL, &token_labels).increment(usage.input_tokens);
        counter!(INFERENCE_OUTPUT_TOKENS_TOTAL, &token_labels).increment(usage.output_tokens);
        counter!(INFERENCE_CACHED_INPUT_TOKENS_TOTAL, &token_labels)
            .increment(usage.cached_input_tokens);
    }
}

pub fn record_tool_call(
    tool_name: &str,
    user_id: &str,
    agent_id: &str,
    duration: Duration,
    outcome: &str,
) {
    let labels = [
        ("tool_name", tool_name.to_string()),
        ("user_id", user_id.to_string()),
        ("agent_id", agent_id.to_string()),
        ("outcome", outcome.to_string()),
    ];

    counter!(TOOL_CALLS_TOTAL, &labels).increment(1);
    histogram!(TOOL_CALL_DURATION_SECONDS, &labels).record(duration.as_secs_f64());
}

pub fn record_policy_evaluation(action: &str, decision: &str, duration: Duration) {
    let labels = [
        ("action", action.to_string()),
        ("decision", decision.to_string()),
    ];

    counter!(POLICY_EVALUATIONS_TOTAL, &labels).increment(1);
    histogram!(POLICY_EVALUATION_DURATION_SECONDS, &labels).record(duration.as_secs_f64());
}
