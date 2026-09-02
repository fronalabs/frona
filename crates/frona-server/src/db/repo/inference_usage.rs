use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use surrealdb::types::SurrealValue;

use crate::core::error::AppError;
use crate::inference::usage::models::UsageRollup;
use crate::inference::usage::{
    BucketLatencyRow, ChatCostRow, InferenceUsage, InferenceUsageRepository, LatencyPercentiles,
    ModelLatencyRow, TimeBucket, UsageBucket,
};

use super::generic::SurrealRepo;

/// Aggregate over a filtered set of rows: SUM tokens, SUM cost, COUNT rows.
///
/// The `<float>` cast on `cost_usd` is load-bearing: SurrealDB's `math::sum`
/// returns a generic `number`, which doesn't deserialize into `f64` via
/// `SurrealValue`. The cast pins the result type so `UsageRollup.cost_usd`
/// reads cleanly even when actual prices are populated.
const ROLLUP_SELECT: &str = "SELECT \
    math::sum(input_tokens) AS input_tokens, \
    math::sum(cached_input_tokens) AS cached_input_tokens, \
    math::sum(output_tokens) AS output_tokens, \
    <float>math::sum(cost_usd ?? 0.0) AS cost_usd, \
    count() AS calls \
    FROM inference_usage";

#[async_trait]
impl InferenceUsageRepository for SurrealRepo<InferenceUsage> {
    async fn aggregate_by_chat(
        &self,
        chat_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<UsageRollup, AppError> {
        let (window_clause, bindings) = window_clause(since, until);
        let query = format!("{ROLLUP_SELECT} WHERE chat_id = $chat_id{window_clause} GROUP ALL");
        let mut req = self
            .db()
            .query(&query)
            .bind(("chat_id", chat_id.to_string()));
        for (k, v) in bindings {
            req = req.bind((k, v));
        }
        let mut result = req.await.map_err(|e| AppError::Database(e.to_string()))?;
        let rollup: Option<UsageRollup> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rollup.unwrap_or_default())
    }

    async fn aggregate_by_user(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<UsageRollup, AppError> {
        let (window_clause, bindings) = window_clause(since, until);
        let query = format!("{ROLLUP_SELECT} WHERE user_id = $user_id{window_clause} GROUP ALL");
        let mut req = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()));
        for (k, v) in bindings {
            req = req.bind((k, v));
        }
        let mut result = req.await.map_err(|e| AppError::Database(e.to_string()))?;
        let rollup: Option<UsageRollup> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rollup.unwrap_or_default())
    }

    async fn aggregate_by_agent(
        &self,
        agent_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<UsageRollup, AppError> {
        let (window_clause, bindings) = window_clause(since, until);
        let query = format!("{ROLLUP_SELECT} WHERE agent_id = $agent_id{window_clause} GROUP ALL");
        let mut req = self
            .db()
            .query(&query)
            .bind(("agent_id", agent_id.to_string()));
        for (k, v) in bindings {
            req = req.bind((k, v));
        }
        let mut result = req.await.map_err(|e| AppError::Database(e.to_string()))?;
        let rollup: Option<UsageRollup> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rollup.unwrap_or_default())
    }

    async fn aggregate_by_kind(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<HashMap<String, UsageRollup>, AppError> {
        let (window_clause, bindings) = window_clause(since, until);
        let query = format!(
            "SELECT \
                kind_tag AS key, \
                math::sum(input_tokens) AS input_tokens, \
                math::sum(cached_input_tokens) AS cached_input_tokens, \
                math::sum(output_tokens) AS output_tokens, \
                <float>math::sum(cost_usd ?? 0.0) AS cost_usd, \
                count() AS calls \
                FROM inference_usage \
                WHERE user_id = $user_id{window_clause} \
                GROUP BY kind_tag"
        );
        let mut req = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()));
        for (k, v) in bindings {
            req = req.bind((k, v));
        }
        let mut result = req.await.map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<GroupedRollup> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let key = r.key.clone();
                (key, r.rollup())
            })
            .collect())
    }

    async fn last_chat_input_tokens(&self, chat_id: &str) -> Result<Option<u64>, AppError> {
        // SurrealDB requires `ORDER BY` columns to be in the SELECT
        // projection — hence `created_at` is selected alongside the value we
        // actually consume.
        #[derive(serde::Deserialize, SurrealValue)]
        #[surreal(crate = "surrealdb::types")]
        struct Row {
            input_tokens: u64,
            #[serde(default)]
            #[allow(dead_code)]
            created_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let mut res = self
            .db()
            .query(
                "SELECT input_tokens, created_at FROM inference_usage \
                 WHERE chat_id = $chat_id \
                   AND (kind_tag = 'Text' OR kind_tag = 'ToolTurn') \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(("chat_id", chat_id.to_string()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let row: Option<Row> = res.take(0).map_err(|e| AppError::Database(e.to_string()))?;
        Ok(row.map(|r| r.input_tokens))
    }

    async fn aggregate_by_model(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<HashMap<String, UsageRollup>, AppError> {
        let (window_clause, bindings) = window_clause(since, until);
        let query = format!(
            "SELECT \
                model_ref AS key, \
                math::sum(input_tokens) AS input_tokens, \
                math::sum(cached_input_tokens) AS cached_input_tokens, \
                math::sum(output_tokens) AS output_tokens, \
                <float>math::sum(cost_usd ?? 0.0) AS cost_usd, \
                count() AS calls \
                FROM inference_usage \
                WHERE user_id = $user_id{window_clause} \
                GROUP BY model_ref"
        );
        let mut req = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()));
        for (k, v) in bindings {
            req = req.bind((k, v));
        }
        let mut result = req.await.map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<GroupedRollup> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let key = r.key.clone();
                (key, r.rollup())
            })
            .collect())
    }

    async fn aggregate_buckets_by_user(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        bucket: TimeBucket,
    ) -> Result<Vec<UsageBucket>, AppError> {
        // `time::floor` anchors each row's `created_at` to the start of its
        // bucket; grouping by that field gives one row per bucket window.
        // Bucket size is a closed enum so the SQL gets a literal duration
        // and SurrealDB can use `idx_iu_user_created` for the range scan.
        let dur = bucket.duration_literal();
        let query = format!(
            "SELECT \
                time::floor(created_at, {dur}) AS bucket, \
                math::sum(input_tokens) AS input_tokens, \
                math::sum(cached_input_tokens) AS cached_input_tokens, \
                math::sum(output_tokens) AS output_tokens, \
                <float>math::sum(cost_usd ?? 0.0) AS cost_usd, \
                count() AS calls \
                FROM inference_usage \
                WHERE user_id = $user_id AND created_at >= $since AND created_at < $until \
                GROUP BY bucket \
                ORDER BY bucket ASC"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .bind(("since", since))
            .bind(("until", until))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<UsageBucket> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rows)
    }

    async fn latency_percentiles_by_user(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<LatencyPercentiles, AppError> {
        #[derive(serde::Deserialize, SurrealValue)]
        #[surreal(crate = "surrealdb::types")]
        struct Row {
            duration_ms_p50: Option<f64>,
            duration_ms_p95: Option<f64>,
            duration_ms_p99: Option<f64>,
            ttft_ms_p50: Option<f64>,
            ttft_ms_p95: Option<f64>,
            ttft_ms_p99: Option<f64>,
        }
        // `math::percentile(array, P)` is NOT an aggregator — it expects an
        // array as the first arg and returns a scalar. We collect each
        // column into an array via a subquery, but use `LET` bindings so
        // we only scan `idx_iu_user_created` *twice* (once for duration,
        // once for ttft) instead of 6× (once per percentile). The
        // `RETURN { … }` evaluates all six percentile calls against the
        // pre-collected arrays.
        let (window_clause, bindings) = window_clause(since, until);
        let query = format!(
            "LET $dur = SELECT VALUE duration_ms FROM inference_usage \
                WHERE user_id = $user_id{window_clause}; \
             LET $ttft = SELECT VALUE ttft_ms FROM inference_usage \
                WHERE user_id = $user_id AND ttft_ms IS NOT NONE{window_clause}; \
             RETURN {{ \
                duration_ms_p50: <float>math::percentile($dur, 50), \
                duration_ms_p95: <float>math::percentile($dur, 95), \
                duration_ms_p99: <float>math::percentile($dur, 99), \
                ttft_ms_p50: <float>math::percentile($ttft, 50), \
                ttft_ms_p95: <float>math::percentile($ttft, 95), \
                ttft_ms_p99: <float>math::percentile($ttft, 99) \
             }};"
        );
        let mut req = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()));
        for (k, v) in bindings {
            req = req.bind((k, v));
        }
        let mut result = req.await.map_err(|e| AppError::Database(e.to_string()))?;
        // Three statements: two LETs (no result rows) + the final RETURN.
        // The RETURN's row is at index 2.
        let row: Option<Row> = result
            .take(2)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(row
            .map(|r| LatencyPercentiles {
                duration_ms_p50: r.duration_ms_p50,
                duration_ms_p95: r.duration_ms_p95,
                duration_ms_p99: r.duration_ms_p99,
                ttft_ms_p50: r.ttft_ms_p50,
                ttft_ms_p95: r.ttft_ms_p95,
                ttft_ms_p99: r.ttft_ms_p99,
            })
            .unwrap_or_default())
    }

    async fn top_chats_by_user(
        &self,
        user_id: &str,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<ChatCostRow>, AppError> {
        let (window_clause, bindings) = window_clause(since, until);
        // SurrealDB stores an absent optional field as NONE, not NULL.
        let query = format!(
            "SELECT \
                chat_id, \
                <float>math::sum(cost_usd ?? 0.0) AS cost_usd, \
                math::sum(input_tokens) AS input_tokens, \
                math::sum(output_tokens) AS output_tokens, \
                count() AS calls \
                FROM inference_usage \
                WHERE user_id = $user_id AND !type::is_none(chat_id){window_clause} \
                GROUP BY chat_id \
                ORDER BY cost_usd DESC \
                LIMIT {limit}"
        );
        let mut req = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()));
        for (k, v) in bindings {
            req = req.bind((k, v));
        }
        let mut result = req.await.map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<ChatCostRow> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(rows)
    }

    async fn latency_by_model(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<ModelLatencyRow>, AppError> {
        // `math::percentile` isn't an aggregator (probed in
        // `probe_per_group_percentile_syntaxes`), so we can't just `GROUP BY
        // model_ref`. Instead:
        //   1. Collect the distinct model_refs in the window.
        //   2. `array::map` over them, running a percentile subquery per
        //      model. Each subquery hits `idx_iu_user_created` for the
        //      user+time range, then filters on `model_ref` in memory —
        //      cheap because the index already narrowed the row set.
        //   3. The result is a JSON array of `{model_ref, *_p50, *_p95,
        //      *_p99}` rows, server-side aggregation, one round-trip.
        let query = "RETURN array::map(\
            (SELECT VALUE model_ref FROM inference_usage \
              WHERE user_id = $user_id \
                AND created_at >= $since AND created_at < $until \
              GROUP BY model_ref), \
            |$m| { \
                model_ref: $m, \
                duration_ms_p50: <float>math::percentile((SELECT VALUE duration_ms FROM inference_usage \
                    WHERE user_id = $user_id AND model_ref = $m \
                      AND created_at >= $since AND created_at < $until), 50), \
                duration_ms_p95: <float>math::percentile((SELECT VALUE duration_ms FROM inference_usage \
                    WHERE user_id = $user_id AND model_ref = $m \
                      AND created_at >= $since AND created_at < $until), 95), \
                duration_ms_p99: <float>math::percentile((SELECT VALUE duration_ms FROM inference_usage \
                    WHERE user_id = $user_id AND model_ref = $m \
                      AND created_at >= $since AND created_at < $until), 99), \
                ttft_ms_p50: <float>math::percentile((SELECT VALUE ttft_ms FROM inference_usage \
                    WHERE user_id = $user_id AND model_ref = $m AND ttft_ms IS NOT NONE \
                      AND created_at >= $since AND created_at < $until), 50), \
                ttft_ms_p95: <float>math::percentile((SELECT VALUE ttft_ms FROM inference_usage \
                    WHERE user_id = $user_id AND model_ref = $m AND ttft_ms IS NOT NONE \
                      AND created_at >= $since AND created_at < $until), 95), \
                ttft_ms_p99: <float>math::percentile((SELECT VALUE ttft_ms FROM inference_usage \
                    WHERE user_id = $user_id AND model_ref = $m AND ttft_ms IS NOT NONE \
                      AND created_at >= $since AND created_at < $until), 99) \
            })";
        let mut result = self
            .db()
            .query(query)
            .bind(("user_id", user_id.to_string()))
            .bind(("since", since))
            .bind(("until", until))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut rows: Vec<ModelLatencyRow> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        for r in &mut rows {
            denan(&mut r.duration_ms_p50);
            denan(&mut r.duration_ms_p95);
            denan(&mut r.duration_ms_p99);
            denan(&mut r.ttft_ms_p50);
            denan(&mut r.ttft_ms_p95);
            denan(&mut r.ttft_ms_p99);
        }
        Ok(rows)
    }

    async fn latency_by_bucket(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        bucket: TimeBucket,
    ) -> Result<Vec<BucketLatencyRow>, AppError> {
        // Same pattern as `latency_by_model` but grouping by
        // `time::floor(created_at, …)` instead of `model_ref`.
        let dur = bucket.duration_literal();
        let query = format!(
            "RETURN array::map(\
                (SELECT VALUE time::floor(created_at, {dur}) AS bucket FROM inference_usage \
                  WHERE user_id = $user_id \
                    AND created_at >= $since AND created_at < $until \
                  GROUP BY bucket \
                  ORDER BY bucket ASC), \
                |$b| {{ \
                    bucket: $b, \
                    duration_ms_p50: <float>math::percentile((SELECT VALUE duration_ms FROM inference_usage \
                        WHERE user_id = $user_id \
                          AND time::floor(created_at, {dur}) = $b \
                          AND created_at >= $since AND created_at < $until), 50), \
                    duration_ms_p95: <float>math::percentile((SELECT VALUE duration_ms FROM inference_usage \
                        WHERE user_id = $user_id \
                          AND time::floor(created_at, {dur}) = $b \
                          AND created_at >= $since AND created_at < $until), 95), \
                    duration_ms_p99: <float>math::percentile((SELECT VALUE duration_ms FROM inference_usage \
                        WHERE user_id = $user_id \
                          AND time::floor(created_at, {dur}) = $b \
                          AND created_at >= $since AND created_at < $until), 99), \
                    ttft_ms_p50: <float>math::percentile((SELECT VALUE ttft_ms FROM inference_usage \
                        WHERE user_id = $user_id AND ttft_ms IS NOT NONE \
                          AND time::floor(created_at, {dur}) = $b \
                          AND created_at >= $since AND created_at < $until), 50), \
                    ttft_ms_p95: <float>math::percentile((SELECT VALUE ttft_ms FROM inference_usage \
                        WHERE user_id = $user_id AND ttft_ms IS NOT NONE \
                          AND time::floor(created_at, {dur}) = $b \
                          AND created_at >= $since AND created_at < $until), 95), \
                    ttft_ms_p99: <float>math::percentile((SELECT VALUE ttft_ms FROM inference_usage \
                        WHERE user_id = $user_id AND ttft_ms IS NOT NONE \
                          AND time::floor(created_at, {dur}) = $b \
                          AND created_at >= $since AND created_at < $until), 99) \
                }})"
        );
        let mut result = self
            .db()
            .query(&query)
            .bind(("user_id", user_id.to_string()))
            .bind(("since", since))
            .bind(("until", until))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut rows: Vec<BucketLatencyRow> = result
            .take(0)
            .map_err(|e| AppError::Database(e.to_string()))?;
        for r in &mut rows {
            denan(&mut r.duration_ms_p50);
            denan(&mut r.duration_ms_p95);
            denan(&mut r.duration_ms_p99);
            denan(&mut r.ttft_ms_p50);
            denan(&mut r.ttft_ms_p95);
            denan(&mut r.ttft_ms_p99);
        }
        Ok(rows)
    }
}

/// `math::percentile([], P)` returns NaN. NaN doesn't serialize to valid
/// JSON and isn't a meaningful percentile — collapse it to `None` so the
/// JSON layer emits `null` and the frontend renders an em-dash.
fn denan(v: &mut Option<f64>) {
    if matches!(v, Some(x) if x.is_nan()) {
        *v = None;
    }
}

fn window_clause(
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> (String, Vec<(&'static str, DateTime<Utc>)>) {
    let mut clause = String::new();
    let mut bindings = Vec::new();
    if let Some(s) = since {
        clause.push_str(" AND created_at >= $since");
        bindings.push(("since", s));
    }
    if let Some(u) = until {
        clause.push_str(" AND created_at < $until");
        bindings.push(("until", u));
    }
    (clause, bindings)
}

/// Generic shape returned by GROUP BY queries — collapses to `(key, UsageRollup)`.
#[derive(Debug, serde::Serialize, serde::Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
struct GroupedRollup {
    #[serde(alias = "kind_tag", alias = "model_ref", alias = "key")]
    key: String,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cost_usd: f64,
    #[serde(default)]
    calls: u64,
}

impl GroupedRollup {
    fn rollup(self) -> UsageRollup {
        UsageRollup {
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: self.cost_usd,
            calls: self.calls,
        }
    }
}
