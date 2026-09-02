//! E2E tests for `UsageService`: assert that every successful provider call
//! produces exactly one row in `inference_usage`, that retry/fallback metadata
//! is recorded correctly, and that the repository aggregation queries return
//! the right shape.
//!
//! These tests use a fresh in-memory SurrealDB per test (no shared state with
//! the rest of the helpers), so row counts and aggregations can be asserted
//! deterministically.

mod helpers;

use std::collections::HashMap;
use std::sync::Arc;

use frona::chat::broadcast::BroadcastService;
use frona::db::repo::generic::SurrealRepo;
use frona::inference::config::{ModelGroup, RetryConfig};
use frona::inference::error::InferenceError;
use frona::inference::metadata::catalog::Cost;
use frona::inference::metadata::{ModelCatalogSnapshot, ModelCatalogStore, ModelEntry};
use frona::inference::provider::{ModelProvider, ModelRef};
use frona::inference::registry::ModelProviderRegistry;
use frona::inference::usage::{
    CompactionTarget, InferenceKind, InferenceUsage, InferenceUsageRepository, TimeBucket,
    UsageContext, UsageService,
};
use frona::inference::{structured_inference, text_inference};
use rig_core::completion::Message as RigMessage;
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;

use helpers::{MockModelProvider, MockResponse, init_metrics};

/// Fresh in-memory DB + bound `UsageService`. Pricing is set for the
/// "mock/test-model" key so cost rows are non-None and the aggregations have
/// something to sum.
async fn fresh_service() -> (Surreal<surrealdb::engine::local::Db>, UsageService) {
    let db = Surreal::new::<Mem>(()).await.expect("test db");
    frona::db::init::setup_schema(&db).await.expect("schema");

    let mut entries = HashMap::new();
    entries.insert(
        "test-model".to_string(),
        ModelEntry {
            // models.dev publishes costs as USD per 1M tokens.
            // $1 / 1M input, $2 / 1M output.
            cost: Some(Cost {
                input: 1.0,
                output: 2.0,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let snapshot = ModelCatalogSnapshot {
        version: "test".to_string(),
        fetched_at: chrono::Utc::now(),
        entries,
        protocol_defaults: std::collections::HashMap::new(),
    };
    let catalog = ModelCatalogStore::new(snapshot);
    let svc = UsageService::new(
        catalog,
        SurrealRepo::<InferenceUsage>::new(db.clone()),
        BroadcastService::new(),
    );
    (db, svc)
}

fn chat_usage_ctx(user: &str, agent: &str, chat: &str, message: &str) -> UsageContext {
    UsageContext::new(
        InferenceKind::Text {
            agent_id: agent.to_string(),
            chat_id: chat.to_string(),
            message_id: message.to_string(),
        },
        user,
        "primary",
    )
}

fn fast_retry_model_group(fallbacks: Vec<ModelRef>) -> ModelGroup {
    ModelGroup {
        name: "primary".into(),
        main: ModelRef {
            provider: "mock".into(),
            model_id: "test-model".into(),
        },
        fallbacks,
        max_tokens: Some(4096),
        temperature: None,
        context_window: 128_000,
        retry: RetryConfig {
            // Two retries so a Rate-Limited-then-success scenario actually retries.
            // 0ms backoff so tests don't pay wallclock for backoff sleeps.
            max_retries: 3,
            initial_backoff_ms: 0,
            backoff_multiplier: 1.0,
            max_backoff_ms: 0,
        },
        inference: Default::default(),
    }
}

fn registry_with(providers: Vec<(&str, Arc<dyn ModelProvider>)>) -> ModelProviderRegistry {
    let map = providers
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    ModelProviderRegistry::for_testing(map, HashMap::new())
}

async fn list_all_rows(db: &Surreal<surrealdb::engine::local::Db>) -> Vec<InferenceUsage> {
    // `meta::id(id)` collapses Surreal's RecordId back to the bare uuid string,
    // matching what `InferenceUsage.id: String` expects.
    let mut result = db
        .query("SELECT *, meta::id(id) AS id FROM inference_usage ORDER BY created_at ASC")
        .await
        .expect("query");
    result.take(0).expect("take")
}

#[tokio::test]
async fn single_success_records_one_row_with_zero_retry_and_no_fallback() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "ok".into(),
    )]));
    let registry = registry_with(vec![("mock", provider.clone() as Arc<dyn ModelProvider>)]);
    let ctx = chat_usage_ctx("u1", "a1", "c1", "m1");

    let out = text_inference(
        &registry,
        &fast_retry_model_group(vec![]),
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(out, "ok");

    let rows = list_all_rows(&db).await;
    assert_eq!(rows.len(), 1, "expected exactly one row");
    let row = &rows[0];
    assert_eq!(row.user_id, "u1");
    assert_eq!(row.chat_id.as_deref(), Some("c1"));
    assert_eq!(row.message_id.as_deref(), Some("m1"));
    assert_eq!(row.kind_tag, "Text");
    assert_eq!(row.model_ref, "mock/test-model");
    assert_eq!(row.fallback_index, 0);
    assert_eq!(row.retry_count, 0);
    assert_eq!(row.retry_overhead_ms, 0);
    assert_eq!(row.input_tokens, 10);
    assert_eq!(row.output_tokens, 5);
    // 10 * 0.000_001 + 5 * 0.000_002 = 0.000_02
    assert!(row.cost_usd.is_some(), "cost should be computed");
    assert!((row.cost_usd.unwrap() - 0.000_02).abs() < 1e-9);
}

#[tokio::test]
async fn retry_then_success_records_retry_count_and_overhead() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::RateLimited {
            retry_after_secs: 0,
        }),
        MockResponse::Error(InferenceError::RateLimited {
            retry_after_secs: 0,
        }),
        MockResponse::Text("recovered".into()),
    ]));
    let registry = registry_with(vec![("mock", provider.clone() as Arc<dyn ModelProvider>)]);
    let ctx = chat_usage_ctx("u1", "a1", "c1", "m1");

    let out = text_inference(
        &registry,
        &fast_retry_model_group(vec![]),
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(out, "recovered");
    assert_eq!(provider.calls(), 3);

    let rows = list_all_rows(&db).await;
    assert_eq!(rows.len(), 1, "still one row — only success is recorded");
    let row = &rows[0];
    assert_eq!(row.retry_count, 2, "two failed attempts before success");
    assert_eq!(row.fallback_index, 0, "main model recovered, no fallback");
}

#[tokio::test]
async fn main_fails_fallback_succeeds_records_fallback_index_and_model_ref() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    let main = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("main down".into()),
    )]));
    let fb = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "ok".into(),
    )]));
    let registry = registry_with(vec![
        ("mock", main as Arc<dyn ModelProvider>),
        ("fallback", fb as Arc<dyn ModelProvider>),
    ]);
    let group = fast_retry_model_group(vec![ModelRef {
        provider: "fallback".into(),
        model_id: "fallback-model".into(),
    }]);
    let ctx = chat_usage_ctx("u1", "a1", "c1", "m1");

    let out = text_inference(
        &registry,
        &group,
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(out, "ok");

    let rows = list_all_rows(&db).await;
    assert_eq!(
        rows.len(),
        1,
        "main failures don't produce rows; only the fallback success does"
    );
    let row = &rows[0];
    assert_eq!(row.fallback_index, 1, "fallback index #1 ran");
    assert_eq!(row.model_ref, "fallback/fallback-model");
}

#[tokio::test]
async fn second_fallback_records_fallback_index_two() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    let main = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("m".into()),
    )]));
    let fb1 = Arc::new(MockModelProvider::new(vec![MockResponse::Error(
        InferenceError::InferenceFailed("fb1".into()),
    )]));
    let fb2 = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "ok".into(),
    )]));
    let registry = registry_with(vec![
        ("mock", main as Arc<dyn ModelProvider>),
        ("fb1", fb1 as Arc<dyn ModelProvider>),
        ("fb2", fb2 as Arc<dyn ModelProvider>),
    ]);
    let group = fast_retry_model_group(vec![
        ModelRef {
            provider: "fb1".into(),
            model_id: "fallback-1".into(),
        },
        ModelRef {
            provider: "fb2".into(),
            model_id: "fallback-2".into(),
        },
    ]);
    let ctx = chat_usage_ctx("u1", "a1", "c1", "m1");

    text_inference(
        &registry,
        &group,
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &ctx,
    )
    .await
    .unwrap();

    let rows = list_all_rows(&db).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].fallback_index, 2);
    assert_eq!(rows[0].model_ref, "fb2/fallback-2");
}

#[tokio::test]
async fn structured_inference_records_row() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct Out {
        #[allow(dead_code)]
        x: i32,
    }

    let provider = Arc::new(MockModelProvider::new(vec![MockResponse::ToolCalls(vec![
        ("id".into(), "submit".into(), serde_json::json!({"x": 1})),
    ])]));
    let registry = registry_with(vec![("mock", provider as Arc<dyn ModelProvider>)]);
    let ctx = chat_usage_ctx("u1", "a1", "c1", "m1");

    let _out: Out = structured_inference(
        &registry,
        &fast_retry_model_group(vec![]),
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &ctx,
    )
    .await
    .unwrap();

    let rows = list_all_rows(&db).await;
    assert_eq!(rows.len(), 1);
    // structured_inference doesn't get a Usage from rig, so we record zeros.
    assert_eq!(rows[0].input_tokens, 0);
    assert_eq!(rows[0].output_tokens, 0);
}

#[tokio::test]
async fn aggregate_by_chat_sums_rows() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("a".into()),
        MockResponse::Text("b".into()),
        MockResponse::Text("c".into()),
    ]));
    let registry = registry_with(vec![("mock", provider.clone() as Arc<dyn ModelProvider>)]);

    // Three calls scoped to the same chat.
    for msg_id in ["m1", "m2", "m3"] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            &chat_usage_ctx("u1", "a1", "c1", msg_id),
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let rollup = repo.aggregate_by_chat("c1", None, None).await.unwrap();
    assert_eq!(rollup.calls, 3);
    assert_eq!(rollup.input_tokens, 30); // 10 * 3
    assert_eq!(rollup.output_tokens, 15); // 5 * 3
    assert!((rollup.cost_usd - 0.000_06).abs() < 1e-9);
}

// SurrealValue does not honor serde aliases, so the query must use
// `SELECT kind_tag AS key`.

#[tokio::test]
async fn aggregate_by_kind_groups_by_kind_tag() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("chat".into()),
        MockResponse::Text("title".into()),
        MockResponse::Text("title2".into()),
    ]));
    let registry = registry_with(vec![("mock", provider.clone() as Arc<dyn ModelProvider>)]);

    let chat_ctx = chat_usage_ctx("u1", "a1", "c1", "m1");
    let title_ctx = UsageContext::new(
        InferenceKind::Title {
            agent_id: "a1".into(),
            chat_id: "c1".into(),
        },
        "u1",
        "primary",
    );

    for ctx in [&chat_ctx, &title_ctx, &title_ctx] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            ctx,
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let by_kind = repo
        .aggregate_by_kind("u1", None, None)
        .await
        .expect("aggregate_by_kind must not 500 — covers the kind_tag AS key fix");
    assert_eq!(by_kind.len(), 2);
    assert_eq!(by_kind.get("Text").unwrap().calls, 1);
    assert_eq!(by_kind.get("Title").unwrap().calls, 2);
}

#[tokio::test]
async fn aggregate_by_model_groups_by_model_ref() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    // Main fails once, fallback succeeds - produces one row on fallback model.
    let main = Arc::new(MockModelProvider::new(vec![
        MockResponse::Error(InferenceError::InferenceFailed("down".into())),
        // Second call succeeds (after the failed first call's retry budget).
        MockResponse::Text("main-ok".into()),
    ]));
    let fb = Arc::new(MockModelProvider::new(vec![MockResponse::Text(
        "fb-ok".into(),
    )]));
    let registry = registry_with(vec![
        ("mock", main as Arc<dyn ModelProvider>),
        ("fallback", fb as Arc<dyn ModelProvider>),
    ]);
    let group = fast_retry_model_group(vec![ModelRef {
        provider: "fallback".into(),
        model_id: "fallback-model".into(),
    }]);

    // Call 1: main retries-exhausted → fallback succeeds. Row on fallback.
    // Call 2: main recovers → row on main.
    text_inference(
        &registry,
        &group,
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &chat_usage_ctx("u1", "a1", "c1", "m1"),
    )
    .await
    .unwrap();
    text_inference(
        &registry,
        &group,
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &chat_usage_ctx("u1", "a1", "c1", "m2"),
    )
    .await
    .unwrap();

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let by_model = repo.aggregate_by_model("u1", None, None).await.unwrap();
    assert!(by_model.contains_key("mock/test-model"));
    assert!(by_model.contains_key("fallback/fallback-model"));
    assert_eq!(by_model["mock/test-model"].calls, 1);
    assert_eq!(by_model["fallback/fallback-model"].calls, 1);
}

#[tokio::test]
async fn aggregate_by_user_totals_across_chats() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("a".into()),
        MockResponse::Text("b".into()),
        MockResponse::Text("c".into()),
    ]));
    let registry = registry_with(vec![("mock", provider.clone() as Arc<dyn ModelProvider>)]);

    for (chat, msg) in [("c1", "m1"), ("c2", "m2"), ("c3", "m3")] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            &chat_usage_ctx("u1", "a1", chat, msg),
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let rollup = repo.aggregate_by_user("u1", None, None).await.unwrap();
    assert_eq!(rollup.calls, 3);
    assert_eq!(rollup.input_tokens, 30);

    // Future-windowed query: nothing should match.
    let future = chrono::Utc::now() + chrono::Duration::hours(1);
    let empty = repo
        .aggregate_by_user("u1", Some(future), None)
        .await
        .unwrap();
    assert_eq!(empty.calls, 0);
    assert_eq!(empty.input_tokens, 0);
}

// SurrealDB rejects ORDER BY columns that are not in the SELECT list. This
// query must project `created_at` while it selects the latest Chat or ToolTurn.

#[tokio::test]
async fn last_chat_input_tokens_returns_latest_main_chat_row() {
    init_metrics();
    let (db, svc) = fresh_service().await;
    let registry = registry_with(vec![(
        "mock",
        Arc::new(MockModelProvider::new(vec![
            MockResponse::Text("a".into()),
            MockResponse::Text("b".into()),
            MockResponse::Text("c".into()),
            MockResponse::Text("d".into()),
        ])) as Arc<dyn ModelProvider>,
    )]);

    // Title-kind first (should be ignored), then two Chat-kind, then a
    // Compaction-kind (also ignored). The most recent Chat call's
    // input_tokens (10 per the mock) is what we expect.
    let title_ctx = UsageContext::new(
        InferenceKind::Title {
            agent_id: "a1".into(),
            chat_id: "c1".into(),
        },
        "u1",
        "primary",
    );
    for ctx in [
        &title_ctx,
        &chat_usage_ctx("u1", "a1", "c1", "m1"),
        &chat_usage_ctx("u1", "a1", "c1", "m2"),
    ] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            ctx,
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let last = repo
        .last_chat_input_tokens("c1")
        .await
        .expect("query must not 500 — regression coverage for the SELECT-ORDER-BY fix");
    assert_eq!(last, Some(10));
}

#[tokio::test]
async fn last_chat_input_tokens_returns_none_when_no_main_chat_rows() {
    init_metrics();
    let (db, svc) = fresh_service().await;
    let registry = registry_with(vec![(
        "mock",
        Arc::new(MockModelProvider::new(vec![MockResponse::Text("t".into())]))
            as Arc<dyn ModelProvider>,
    )]);

    // Only a Title row; no Chat/ToolTurn - last_chat_input_tokens must be None.
    let title_ctx = UsageContext::new(
        InferenceKind::Title {
            agent_id: "a1".into(),
            chat_id: "c1".into(),
        },
        "u1",
        "primary",
    );
    text_inference(
        &registry,
        &fast_retry_model_group(vec![]),
        "sys",
        vec![RigMessage::user("hi")],
        &svc,
        &title_ctx,
    )
    .await
    .unwrap();

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    assert_eq!(repo.last_chat_input_tokens("c1").await.unwrap(), None);
    // Unknown chat id also returns None.
    assert_eq!(
        repo.last_chat_input_tokens("nonexistent").await.unwrap(),
        None
    );
}

// SurrealDB can return `math::percentile` as an array when it receives a scalar
// percentile. Probe the raw shape so a failure points to the SQL.

#[tokio::test]
async fn percentile_query_returns_scalars_after_array_unwrap() {
    init_metrics();
    let (db, svc) = fresh_service().await;

    // Drive 5 calls so each percentile has data to compute against.
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("a".into()),
        MockResponse::Text("b".into()),
        MockResponse::Text("c".into()),
        MockResponse::Text("d".into()),
        MockResponse::Text("e".into()),
    ]));
    let registry = registry_with(vec![("mock", provider as Arc<dyn ModelProvider>)]);
    for msg_id in ["m1", "m2", "m3", "m4", "m5"] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            &chat_usage_ctx("u1", "a1", "c1", msg_id),
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let p = repo
        .latency_percentiles_by_user("u1", None, None)
        .await
        .expect("percentile query must succeed");

    // With 5 calls of duration_ms ≈ 0 (mock fires synchronously), p50/p95/p99
    // all deserialize cleanly as `Some(0.0)`. The assertion is on shape
    // (didn't deserialization blow up), not on numeric value.
    assert!(
        p.duration_ms_p50.is_some(),
        "expected duration_ms_p50 to deserialize as Some(f64), got {p:?}"
    );
    assert!(
        p.duration_ms_p95.is_some(),
        "expected duration_ms_p95 to deserialize as Some(f64), got {p:?}"
    );
    assert!(
        p.duration_ms_p99.is_some(),
        "expected duration_ms_p99 to deserialize as Some(f64), got {p:?}"
    );
    // ttft_ms is None on the mock provider (non-streaming path), so the
    // ttft percentiles should be None - but the query MUST NOT 500 on the
    // empty subquery, which is the regression we're guarding against.
    // (Previous bug: passing scalar duration_ms to math::percentile caused
    // per-row evaluation returning an array of nulls.)
}

#[tokio::test]
async fn latency_by_model_computes_percentiles_in_sql() {
    init_metrics();
    let (db, svc) = fresh_service().await;
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("a".into()),
        MockResponse::Text("b".into()),
        MockResponse::Text("c".into()),
    ]));
    let registry = registry_with(vec![("mock", provider as Arc<dyn ModelProvider>)]);
    for msg_id in ["m1", "m2", "m3"] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            &chat_usage_ctx("u1", "a1", "c1", msg_id),
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let since = chrono::Utc::now() - chrono::Duration::hours(1);
    let until = chrono::Utc::now() + chrono::Duration::hours(1);
    let rows = repo
        .latency_by_model("u1", since, until)
        .await
        .expect("latency_by_model query");

    assert_eq!(rows.len(), 1, "one entry per distinct model_ref");
    let r = &rows[0];
    assert_eq!(r.model_ref, "mock/test-model");
    // Durations are recorded - should produce a percentile (even if 0).
    assert!(
        r.duration_ms_p50.is_some(),
        "expected duration p50 deserialized as Some(f64), got {r:?}"
    );
    assert!(r.duration_ms_p95.is_some(), "got {r:?}");
    assert!(r.duration_ms_p99.is_some(), "got {r:?}");
    // text_inference is non-streaming → ttft is always None on the row →
    // subquery returns empty array → math::percentile(empty, P) yields null,
    // which deserializes as None. Regression guard: must NOT 500.
    assert!(r.ttft_ms_p50.is_none(), "got {r:?}");
}

#[tokio::test]
async fn latency_by_bucket_computes_percentiles_in_sql() {
    init_metrics();
    let (db, svc) = fresh_service().await;
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("a".into()),
        MockResponse::Text("b".into()),
        MockResponse::Text("c".into()),
    ]));
    let registry = registry_with(vec![("mock", provider as Arc<dyn ModelProvider>)]);
    for msg_id in ["m1", "m2", "m3"] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            &chat_usage_ctx("u1", "a1", "c1", msg_id),
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let since = chrono::Utc::now() - chrono::Duration::hours(1);
    let until = chrono::Utc::now() + chrono::Duration::hours(1);
    let rows = repo
        .latency_by_bucket("u1", since, until, TimeBucket::Hour)
        .await
        .expect("latency_by_bucket query");

    // All three calls land in the same hour bucket - should be one entry.
    assert_eq!(rows.len(), 1);
    assert!(rows[0].duration_ms_p50.is_some(), "got {:?}", rows[0]);
    assert!(rows[0].duration_ms_p95.is_some(), "got {:?}", rows[0]);
    assert!(rows[0].duration_ms_p99.is_some(), "got {:?}", rows[0]);
}

#[tokio::test]
async fn top_chats_by_user_skips_rootless_rows() {
    init_metrics();
    let (db, svc) = fresh_service().await;
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("a".into()),
        MockResponse::Text("b".into()),
        MockResponse::Text("c".into()),
        MockResponse::Text("d".into()),
    ]));
    let registry = registry_with(vec![("mock", provider as Arc<dyn ModelProvider>)]);

    let memory_ctx = UsageContext::new(InferenceKind::Memory, "u1", "primary");
    let user_compaction_ctx = UsageContext::new(
        InferenceKind::Compaction {
            target: CompactionTarget::User,
        },
        "u1",
        "primary",
    );
    for ctx in [
        &chat_usage_ctx("u1", "a1", "c1", "m1"),
        &chat_usage_ctx("u1", "a1", "c2", "m2"),
        &memory_ctx,
        &user_compaction_ctx,
    ] {
        text_inference(
            &registry,
            &fast_retry_model_group(vec![]),
            "sys",
            vec![RigMessage::user("hi")],
            &svc,
            ctx,
        )
        .await
        .unwrap();
    }

    let repo: SurrealRepo<InferenceUsage> = SurrealRepo::new(db.clone());
    let rows = repo
        .top_chats_by_user("u1", None, None, 10)
        .await
        .expect("top chats query must ignore rows without a chat");

    let mut ids: Vec<&str> = rows.iter().map(|row| row.chat_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["c1", "c2"]);
    assert!(rows.iter().all(|row| row.calls == 1));
}
