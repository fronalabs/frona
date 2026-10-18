use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use surrealdb::types::SurrealValue;

use crate::core::error::AppError;
use crate::core::repository::Repository;

use super::models::{InferenceUsage, UsageRollup};

#[async_trait]
pub trait InferenceUsageRepository: Repository<InferenceUsage> {
    async fn aggregate_by_chat(
        &self,
        chat_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<UsageRollup, AppError>;

    async fn aggregate_by_user(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<UsageRollup, AppError>;

    async fn aggregate_by_agent(
        &self,
        agent_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<UsageRollup, AppError>;

    async fn aggregate_by_kind(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<HashMap<String, UsageRollup>, AppError>;

    async fn aggregate_by_model(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<HashMap<String, UsageRollup>, AppError>;

    /// Latest conversational provider call, including the response/turn anchor.
    async fn last_chat_usage(&self, chat_id: &str) -> Result<Option<InferenceUsage>, AppError>;

    /// `input_tokens` of the latest `Chat` / `ToolTurn` row in the chat —
    /// used to rehydrate "context used so far" after a page reload before
    /// the next live SSE `usage_recorded` event fires.
    async fn last_chat_input_tokens(&self, chat_id: &str) -> Result<Option<u64>, AppError>;

    async fn aggregate_buckets_by_user(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        bucket: TimeBucket,
    ) -> Result<Vec<UsageBucket>, AppError>;

    /// p50/p95/p99 of `duration_ms` and `ttft_ms` for the window. `None` for
    /// `ttft_ms` percentiles when no streaming row exists in the window.
    async fn latency_percentiles_by_user(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<LatencyPercentiles, AppError>;

    async fn top_chats_by_user(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<ChatCostRow>, AppError>;

    async fn latency_by_model(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<ModelLatencyRow>, AppError>;

    async fn latency_by_bucket(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        bucket: TimeBucket,
    ) -> Result<Vec<BucketLatencyRow>, AppError>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ModelLatencyRow {
    pub model_ref: String,
    pub duration_ms_p50: Option<f64>,
    pub duration_ms_p95: Option<f64>,
    pub duration_ms_p99: Option<f64>,
    pub ttft_ms_p50: Option<f64>,
    pub ttft_ms_p95: Option<f64>,
    pub ttft_ms_p99: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct BucketLatencyRow {
    pub bucket: DateTime<Utc>,
    pub duration_ms_p50: Option<f64>,
    pub duration_ms_p95: Option<f64>,
    pub duration_ms_p99: Option<f64>,
    pub ttft_ms_p50: Option<f64>,
    pub ttft_ms_p95: Option<f64>,
    pub ttft_ms_p99: Option<f64>,
}

/// Closed set so SurrealDB's `time::floor` always gets a literal it can
/// index against `idx_iu_user_created`.
#[derive(Debug, Clone, Copy)]
pub enum TimeBucket {
    Hour,
    Day,
}

impl TimeBucket {
    pub fn duration_literal(&self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "1d",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct UsageBucket {
    pub bucket: DateTime<Utc>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub calls: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LatencyPercentiles {
    pub duration_ms_p50: Option<f64>,
    pub duration_ms_p95: Option<f64>,
    pub duration_ms_p99: Option<f64>,
    pub ttft_ms_p50: Option<f64>,
    pub ttft_ms_p95: Option<f64>,
    pub ttft_ms_p99: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ChatCostRow {
    pub chat_id: String,
    pub cost_usd: f64,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
