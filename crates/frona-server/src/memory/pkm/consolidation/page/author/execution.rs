use super::*;
use crate::{
    agent::{models::Agent, prompt::PromptLoader},
    auth::models::User,
    core::repository::Repository,
    db::repo::generic::SurrealRepo,
};
use crate::{
    db::repo::pkm::PkmRepo,
    memory::pkm::{
        consolidation::{ConsolidationScope, inference::ConsolidationInference},
        model::{EntityCategory, MemoryKind},
        storage::PkmStorage,
    },
};
use chrono::Utc;
use frona_harness::{
    BudgetLimits, BudgetOperation, BudgetPermit, BudgetRefusal, ExecutionStop, Handle,
    config::{ProviderModel, RetryConfig},
    inference::{
        InferenceError, ModelConfig, Usage,
        provider::{InferenceOutput, ModelProvider, StreamToken, group::ModelGroup},
    },
};
use rig_core::completion::{AssistantContent, Message, request::ToolDefinition};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem},
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

struct Script {
    outputs: Mutex<VecDeque<Result<Vec<AssistantContent>, InferenceError>>>,
    calls: AtomicUsize,
    entered: Semaphore,
    release: Vec<Semaphore>,
}
impl Script {
    fn new(outputs: Vec<Result<Vec<AssistantContent>, InferenceError>>, gated: usize) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into()),
            calls: AtomicUsize::new(0),
            entered: Semaphore::new(0),
            release: (0..gated).map(|_| Semaphore::new(0)).collect(),
        })
    }
}
#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn ensure_usable(&self, model: &ModelConfig) -> Result<(), InferenceError> {
        if model.model_id == "invalid-configuration" {
            return Err(InferenceError::ConfigError(
                "invalid fixture configuration".into(),
            ));
        }
        Ok(())
    }

    async fn inference(
        &self,
        _: &ModelConfig,
        _: &str,
        _: Vec<Message>,
        tools: Vec<ToolDefinition>,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|tool| tool.name == "search_entities"));
        assert!(tools.iter().any(|tool| tool.name == "read_entity"));
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call");
        self.entered.add_permits(1);
        if let Some(gate) = self.release.get(call) {
            gate.acquire().await.unwrap().forget();
        }
        Ok(InferenceOutput::new(
            output?,
            Usage::new()
                .input_tokens(Some(10))
                .output_tokens(Some(5))
                .total_tokens(Some(15)),
        ))
    }
    async fn stream_inference(
        &self,
        _: &ModelConfig,
        _: &str,
        _: Vec<Message>,
        _: Vec<ToolDefinition>,
        _: tokio::sync::mpsc::Sender<StreamToken>,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        unreachable!()
    }
    async fn structured_inference(
        &self,
        _: &ModelConfig,
        _: &str,
        _: Vec<Message>,
        _: serde_json::Value,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        unreachable!()
    }
}
fn article() -> Result<Vec<AssistantContent>, InferenceError> {
    Ok(vec![AssistantContent::text(
        "# Authored\n\nAn authored article.",
    )])
}
struct Fixture {
    app: crate::app_state_fixture::Fixture,
    db: Surreal<Db>,
    author: PageAuthor,
    cancel: CancellationToken,
}
async fn fixture(provider: Arc<Script>) -> Fixture {
    fixture_with_group(provider, |_| {}).await
}
async fn fixture_with_group(
    provider: Arc<Script>,
    configure: impl FnOnce(&mut ModelGroup),
) -> Fixture {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let app = crate::app_state_fixture::build(&db).await;
    SurrealRepo::<User>::new(db.clone())
        .create(&User {
            id: "u1".into(),
            handle: crate::handle!("owner"),
            email: "owner@example.test".into(),
            name: "Owner".into(),
            password_hash: String::new(),
            timezone: None,
            groups: vec![],
            deactivated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    SurrealRepo::<Agent>::new(db.clone())
        .create(&Agent {
            id: "a1".into(),
            user_id: "u1".into(),
            handle: crate::handle!("agent"),
            name: "Agent".into(),
            description: String::new(),
            model_group: "prepared".into(),
            enabled: true,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            skills: None,
            avatar: None,
            identity: Default::default(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    let mut group = ModelGroup {
        name: "prepared".into(),
        main: ModelConfig {
            catalog_provider: String::new(),
            provider_handle: Handle::const_validated("fixture"),
            model_id: "pkm".into(),
            provider: ProviderModel::Generic,
            request_settings: Default::default(),
        },
        fallbacks: vec![],
        max_tokens: None,
        temperature: None,
        context_window: 128_000,
        retry: RetryConfig {
            max_retries: 0,
            ..Default::default()
        },
        inference: Default::default(),
        providers: Arc::new([("fixture".into(), provider as Arc<dyn ModelProvider>)].into()),
    };
    configure(&mut group);
    let cancel = CancellationToken::new();
    let llm = ConsolidationInference::with_cancel_token(
        app.state.harness.clone(),
        group,
        PromptLoader::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
        ),
        "u1".into(),
        cancel.clone(),
    );
    let repo = Arc::new(PkmRepo::new(db.clone(), 8));
    let storage = PkmStorage::new(app.state.storage_service.clone());
    let scope = ConsolidationScope {
        user_id: "u1".into(),
        user_name: "Owner".into(),
        agent_id: "a1".into(),
        chat_id: None,
        vault: storage
            .vault_scope(crate::handle!("owner"), "Memory")
            .unwrap(),
        temporal_sources: vec![],
        evidence_sources: vec![],
        recall: Default::default(),
        timezone: "UTC".into(),
    };
    let ctx = Arc::new(ConsolidationContext::detached(scope, repo, storage, llm));
    Fixture {
        app,
        db,
        author: PageAuthor {
            ctx,
            prefixes: Default::default(),
            concurrency: 2,
        },
        cancel,
    }
}
impl Fixture {
    async fn seed(&self, path: &str, memory: bool) {
        self.author
            .ctx
            .repo
            .upsert_entity_skeleton(
                "u1",
                path,
                EntityCategory::Concept,
                &[],
                path,
                "Page description",
                &[],
            )
            .await
            .unwrap();
        if memory {
            self.author
                .ctx
                .repo
                .create_sourced_memory(
                    "u1",
                    MemoryKind::Fact,
                    "An established fact.",
                    &[path.into()],
                    vec![],
                )
                .await
                .unwrap();
        }
    }
    async fn dirty(&self) -> Vec<String> {
        let mut paths = self
            .author
            .ctx
            .repo
            .entities_needing_reconciliation("u1")
            .await
            .unwrap();
        paths.sort();
        paths
    }
    async fn assert_unwritten(&self, path: &str) {
        let page = self
            .author
            .ctx
            .repo
            .entity_by_path("u1", path)
            .await
            .unwrap()
            .unwrap();
        assert!(page.sync_content.is_none());
        assert!(page.updated_at > page.rendered_at);
        assert!(
            self.author
                .ctx
                .storage
                .read_page(&self.author.ctx.scope.vault, path)
                .is_none()
        );
    }
}
struct ObservedBudget {
    inner: DefaultBudget,
    refused: Notify,
}
impl Budget for ObservedBudget {
    fn admit(&self, operation: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
        let admitted = self.inner.admit(operation);
        if admitted.is_err() {
            self.refused.notify_one();
        }
        admitted
    }
}

#[tokio::test]
async fn concurrent_pages_share_admission_and_retry_only_unfinished_pages() {
    let script = Script::new(vec![article(), article(), article()], 2);
    let f = fixture(script.clone()).await;
    for path in ["one", "two", "three"] {
        f.seed(path, true).await;
    }
    let budget = Arc::new(ObservedBudget {
        inner: DefaultBudget::new(BudgetLimits {
            tokens: Some(4096),
            ..Default::default()
        }),
        refused: Notify::new(),
    });
    let mut stats = PageAuthorOutcome::default();
    let paths = vec!["one".into(), "two".into(), "three".into()];
    let batch = f.author.author(&paths, &mut stats, budget.clone());
    let controller = async {
        script.entered.acquire_many(2).await.unwrap().forget();
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
        script.release[0].add_permits(1);
        // The first completed response consumes the allowance; the third page is
        // refused while the second already-admitted provider call is still blocked.
        budget.refused.notified().await;
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
        script.release[1].add_permits(1);
    };
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(batch, controller);
    })
    .await
    .unwrap();
    assert_eq!(stats.pages_built, 2);
    assert_eq!(budget.inner.snapshot().model_attempts, 2);
    // ModelGroup retains LegacyNormalized provenance, so default policy charges
    // its conservative minimum even when the provider supplies nonzero usage.
    assert_eq!(budget.inner.snapshot().tokens, 8192);
    let dirty = f.dirty().await;
    assert_eq!(dirty, ["three"]);
    f.assert_unwritten("three").await;
    let first = f
        .author
        .ctx
        .repo
        .entity_by_path("u1", "one")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.author.run(&dirty).await.pages_built, 1);
    assert!(f.dirty().await.is_empty());
    assert_eq!(script.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        f.author
            .ctx
            .repo
            .entity_by_path("u1", "one")
            .await
            .unwrap()
            .unwrap()
            .rendered_at,
        first.rendered_at
    );
}

#[tokio::test]
async fn terminal_page_failure_is_isolated_and_never_replaced() {
    for stop in [
        ExecutionStop::Cancelled("cancelled page".into()),
        ExecutionStop::BudgetExhausted(BudgetRefusal::Denied("exhausted page".into())),
        ExecutionStop::Storage(Box::new(std::io::Error::other("required write failed"))),
    ] {
        let script = Script::new(vec![Err(stop.into()), article()], 0);
        let f = fixture(script.clone()).await;
        for path in ["one", "two"] {
            f.seed(path, true).await;
        }
        let result = f.author.run(&["one".into(), "two".into()]).await;
        assert_eq!(result.pages_built, 1);
        let dirty = f.dirty().await;
        assert_eq!(dirty.len(), 1);
        f.assert_unwritten(&dirty[0]).await;
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn ordinary_provider_failure_and_empty_output_allow_domain_replacement() {
    for output in [
        Err(InferenceError::InferenceFailed("unavailable".into())),
        Ok(vec![AssistantContent::text("  ")]),
    ] {
        let f = fixture(Script::new(vec![output], 0)).await;
        f.seed("one", true).await;
        assert_eq!(f.author.run(&["one".into()]).await.pages_built, 1);
        assert!(f.dirty().await.is_empty());
        let page = f
            .author
            .ctx
            .repo
            .entity_by_path("u1", "one")
            .await
            .unwrap()
            .unwrap();
        assert!(page.body.contains("An established fact."));
        assert_eq!(
            page.sync_content,
            f.author
                .ctx
                .storage
                .read_page(&f.author.ctx.scope.vault, "one")
        );
    }
}

#[tokio::test]
async fn cancelled_no_memory_page_cannot_write_deterministic_content() {
    let script = Script::new(vec![], 0);
    let f = fixture(script.clone()).await;
    f.seed("one", false).await;
    f.cancel.cancel();
    assert_eq!(f.author.run(&["one".into()]).await.pages_built, 0);
    f.assert_unwritten("one").await;
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_required_memory_read_leaves_canonical_page_dirty() {
    let script = Script::new(vec![], 0);
    let mut f = fixture(script.clone()).await;
    f.seed("one", true).await;
    let original = f.author.ctx.repo.clone();
    // Keep the readable entity view, but fail the required memory/link repository.
    Arc::get_mut(&mut f.author.ctx).unwrap().repo =
        Arc::new(PkmRepo::new(Surreal::new::<Mem>(()).await.unwrap(), 8));
    assert_eq!(f.author.run(&["one".into()]).await.pages_built, 0);
    Arc::get_mut(&mut f.author.ctx).unwrap().repo = original;
    f.assert_unwritten("one").await;
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_projection_keeps_durable_content_and_recovery_needs_no_model() {
    let script = Script::new(vec![article()], 0);
    let f = fixture(script.clone()).await;
    f.seed("one", true).await;
    let root = f
        .author
        .ctx
        .storage
        .memory_root(&crate::handle!("owner"))
        .join("Memory");
    std::fs::create_dir_all(&root).unwrap();
    let blocked = root.join("one.md");
    std::fs::create_dir(&blocked).unwrap();
    // Preserve the existing stage count contract: a failed mirror is not counted,
    // although canonical content and its clean marker have already committed.
    assert_eq!(f.author.run(&["one".into()]).await.pages_built, 0);
    assert!(f.dirty().await.is_empty());
    let page = f
        .author
        .ctx
        .repo
        .entity_by_path("u1", "one")
        .await
        .unwrap()
        .unwrap();
    assert!(page.body.contains("An authored article."));
    assert!(page.sync_content.is_some());
    std::fs::remove_dir(blocked).unwrap();
    let report = crate::memory::pkm::recovery::reconcile_user_files(
        &f.author.ctx.repo,
        &f.author.ctx.storage,
        &f.app.state.user_service,
        "u1",
        std::slice::from_ref(&page),
    )
    .await
    .unwrap();
    assert_eq!(report.rerendered, 1);
    assert_eq!(
        page.sync_content,
        f.author
            .ctx
            .storage
            .read_page(&f.author.ctx.scope.vault, "one")
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canonical_commit_failure_does_not_clear_dirty_or_write_a_projection() {
    let f = fixture(Script::new(vec![article()], 0)).await;
    f.seed("one", true).await;
    f.db.query("DEFINE EVENT reject_authored ON TABLE knowledge_entity WHEN $after.sync_content != NONE THEN { THROW 'required canonical commit failed'; }").await.unwrap().check().unwrap();
    assert_eq!(f.author.run(&["one".into()]).await.pages_built, 0);
    f.assert_unwritten("one").await;
}

#[tokio::test]
async fn cancellation_during_model_call_leaves_page_dirty() {
    let script = Script::new(vec![article()], 1);
    let f = fixture(script.clone()).await;
    f.seed("one", true).await;
    let cancel = async {
        script.entered.acquire().await.unwrap().forget();
        f.cancel.cancel();
    };
    let paths = ["one".into()];
    let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(f.author.run(&paths), cancel)
    })
    .await
    .unwrap();
    assert_eq!(outcome.pages_built, 0);
    f.assert_unwritten("one").await;
}

#[tokio::test]
async fn failed_required_links_read_cannot_replace_the_page() {
    let script = Script::new(vec![], 0);
    let f = fixture(script.clone()).await;
    f.seed("one", true).await;
    // Simulate an unreadable stored link while the page and memory reads succeed.
    f.db.query("CREATE knowledge_entity_link:broken SET user_id = 'u1', from_entity_path = 'one', relation = 7").await.unwrap().check().unwrap();
    assert!(
        f.author
            .ctx
            .repo
            .memories_for_entity("u1", "one")
            .await
            .is_ok()
    );
    assert!(
        f.author
            .ctx
            .repo
            .links_from_entity("u1", "one")
            .await
            .is_err()
    );
    assert_eq!(f.author.run(&["one".into()]).await.pages_built, 0);
    f.assert_unwritten("one").await;
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
}

fn read_call(index: usize) -> Result<Vec<AssistantContent>, InferenceError> {
    use rig_core::completion::message::{ToolCall, ToolFunction};
    Ok(vec![AssistantContent::ToolCall(ToolCall::from_wire(
        format!("read-{index}"),
        ToolFunction::new(
            "read_entity".try_into().unwrap(),
            serde_json::json!({"path":"one"}),
        ),
    ))])
}

#[tokio::test]
async fn page_tools_share_admission_and_the_response_cap_never_becomes_replacement() {
    for (outputs, tool_limit, expected_models, expected_tools) in [
        (vec![read_call(0), article()], 1, 2, 1),
        (vec![read_call(0)], 0, 1, 0),
        ((0..20).map(read_call).collect(), 20, 20, 20),
    ] {
        let script = Script::new(outputs, 0);
        let f = fixture(script.clone()).await;
        f.seed("one", true).await;
        let budget = Arc::new(DefaultBudget::new(BudgetLimits {
            tool_calls: Some(tool_limit),
            ..Default::default()
        }));
        let mut stats = PageAuthorOutcome::default();
        f.author
            .author(&["one".into()], &mut stats, budget.clone())
            .await;
        assert_eq!(script.calls.load(Ordering::SeqCst), expected_models);
        assert_eq!(budget.snapshot().tool_calls, expected_tools);
        if expected_models == 2 {
            assert_eq!(stats.pages_built, 1);
            assert!(f.dirty().await.is_empty());
        } else {
            assert_eq!(stats.pages_built, 0);
            f.assert_unwritten("one").await;
        }
    }
}

#[tokio::test]
async fn prepared_group_setup_failure_cannot_replace_or_clean_the_page() {
    for missing_provider in [true, false] {
        let script = Script::new(vec![], 0);
        let f = fixture_with_group(script.clone(), |group| {
            if missing_provider {
                group.providers = Arc::new(Default::default());
            } else {
                group.main.model_id = "invalid-configuration".into();
            }
        })
        .await;
        f.seed("one", true).await;
        let budget = Arc::new(DefaultBudget::default());
        let mut stats = PageAuthorOutcome::default();
        f.author
            .author(&["one".into()], &mut stats, budget.clone())
            .await;
        assert_eq!(stats.pages_built, 0);
        f.assert_unwritten("one").await;
        assert_eq!(script.calls.load(Ordering::SeqCst), 0);
        assert_eq!(budget.snapshot().model_attempts, 0);
    }
}

#[test]
fn replacement_requires_every_nested_provider_failure_to_be_operational() {
    let failed = |source, model: &str, retry_count| InferenceError::ModelFailed {
        provider: "fixture".into(),
        model: model.into(),
        retry_count,
        source: Box::new(source),
    };
    for disallowed in [
        InferenceError::ProviderNotConfigured("missing".into()),
        InferenceError::ModelGroupNotFound("missing".into()),
        InferenceError::ConfigError("invalid".into()),
        InferenceError::Stop(ExecutionStop::Storage(Box::new(std::io::Error::other(
            "required storage",
        )))),
        InferenceError::Cancelled("cancelled".into()),
        InferenceError::AllFallbacksFailed(vec![]),
    ] {
        let nested = InferenceError::AllFallbacksFailed(vec![
            InferenceError::InferenceFailed("unavailable".into()),
            failed(
                InferenceError::AllFallbacksFailed(vec![failed(disallowed, "pkm", 0)]),
                "fallback",
                0,
            ),
        ]);
        assert!(!provider_replacement_allowed(&nested));
    }
    assert!(provider_replacement_allowed(
        &InferenceError::AllFallbacksFailed(vec![
            failed(
                InferenceError::InferenceFailed("unavailable".into()),
                "primary",
                1
            ),
            failed(
                InferenceError::AllFallbacksFailed(vec![InferenceError::EmptyResponse]),
                "fallback",
                0
            ),
        ])
    ));
}
