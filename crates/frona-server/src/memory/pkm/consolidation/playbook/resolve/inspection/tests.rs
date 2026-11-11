use super::*;
use crate::memory::pkm::consolidation::playbook::tools::{PlaybookLookup, ReadPlaybookTool};
use crate::memory::pkm::{
    consolidation::{ConsolidationScope, inference::ConsolidationInference, tools::EntityToolView},
    storage::PkmStorage,
};
use crate::tool::{
    InferenceContext,
    sdk_adapter::{AgentToolAdapter, content_result},
};
use frona_harness::{
    Capabilities, Handle, ModelAdapter, ModelOutput, ModelRequest, UsageReport,
    config::ProviderModel,
    inference::{InferenceError, ModelConfig, ModelGroup, provider::StreamToken},
};
use frona_text::prompt::PromptLoader;
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, atomic::AtomicUsize},
};

struct Fixture {
    _app: crate::app_state_fixture::Fixture,
    inspection: Arc<PlaybookInspection>,
    tool: ReadPlaybookTool,
    context: InferenceContext,
}
async fn fixture() -> Fixture {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let app = crate::app_state_fixture::build(&db).await;
    let repo = Arc::new(crate::db::repo::pkm::PkmRepo::new(db, 8));
    let storage = PkmStorage::new(app.state.storage_service.clone());
    let scope = ConsolidationScope {
        user_id: "u".into(),
        user_name: "Owner".into(),
        agent_id: "a".into(),
        chat_id: None,
        vault: storage
            .vault_scope(crate::handle!("owner"), "memory")
            .unwrap(),
        temporal_sources: vec![],
        evidence_sources: vec![],
        recall: Default::default(),
        timezone: "UTC".into(),
    };
    let prompts = PromptLoader::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/prompts"),
    );
    let group = ModelGroup {
        name: "unused".into(),
        main: ModelConfig {
            catalog_provider: String::new(),
            provider_handle: Handle::const_validated("unused"),
            model_id: "unused".into(),
            provider: ProviderModel::Generic,
            request_settings: Default::default(),
        },
        fallbacks: vec![],
        max_tokens: None,
        temperature: None,
        context_window: 128_000,
        retry: Default::default(),
        inference: Default::default(),
        providers: Arc::new(HashMap::new()),
    };
    let llm = ConsolidationInference::new(
        app.state.harness.clone(),
        group,
        prompts.clone(),
        "u".into(),
    );
    let ctx = Arc::new(ConsolidationContext::detached(
        scope,
        repo.clone(),
        storage,
        llm,
    ));
    let lookup = Arc::new(PlaybookLookup::default());
    let inspection = Arc::new(PlaybookInspection {
        ctx: ctx.clone(),
        ids: PromptIds::new("m", ["memory-one".into()]),
        allowed: BTreeSet::from(["memory-one".into()]),
        existing_by_path: BTreeMap::from([("playbooks/target".into(), EntityCategory::Playbook)]),
        playbook_lookup: lookup.clone(),
        staged_paths: BTreeSet::new(),
    });
    let mut row = KnowledgeConsolidationEntity::pending(
        "run",
        "u",
        "playbooks/target",
        EntityCategory::Playbook,
        vec![],
        Default::default(),
    );
    row.name = "Target".into();
    row.description = "The reusable procedure".into();
    let tool = ReadPlaybookTool {
        prompts,
        repo: ctx.view.clone(),
        memories: repo,
        user_id: "u".into(),
        view: Some(Arc::new(EntityToolView::new(
            vec![row.as_knowledge_entity()],
            usize::MAX,
            Arc::new(AtomicUsize::new(0)),
        ))),
        lookup,
        total: Arc::new(AtomicUsize::new(20)),
    };
    let now = chrono::Utc::now();
    let context = InferenceContext::new_detached(
        serde_json::from_value(json!({"id":"u", "handle":"owner", "email":"owner@example.com", "name":"Owner", "password_hash":"", "created_at":now, "updated_at":now})).unwrap(),
        serde_json::from_value(json!({"id":"a", "user_id":"u", "handle":"agent", "name":"Agent", "description":"", "model_group":"unused", "enabled":true, "created_at":now, "updated_at":now})).unwrap(),
        Default::default(), Default::default(),
    );
    Fixture {
        _app: app,
        inspection,
        tool,
        context,
    }
}

struct Script {
    responses: Mutex<VecDeque<Vec<AssistantContent>>>,
    requests: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl ModelAdapter for Script {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: Option<tokio::sync::mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.requests
            .lock()
            .unwrap()
            .push(format!("{:?}", request.messages));
        Ok(ModelOutput::new(
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected model call"),
            UsageReport::Unavailable,
        ))
    }
}
fn harness(
    responses: Vec<Vec<AssistantContent>>,
) -> (frona_harness::Harness<'static>, Arc<Mutex<Vec<String>>>) {
    let requests = Arc::new(Mutex::new(vec![]));
    let model = frona_harness::Model::from_adapter(
        "fixture",
        "playbook",
        Script {
            responses: Mutex::new(responses.into()),
            requests: requests.clone(),
        },
    )
    .unwrap();
    (frona_harness::Harness::new(model), requests)
}
fn call(name: &str, args: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        name,
        ToolFunction::new(name.try_into().unwrap(), args),
    ))
}
fn target() -> Value {
    json!({"existing_path":" /playbooks/target.md ", "path":" /playbooks/target.md ", "name":"Target", "description":"The reusable procedure", "memory_ids":["m1"]})
}
fn submission(targets: Vec<Value>) -> Vec<AssistantContent> {
    vec![call("submit", json!({"playbooks": targets}))]
}
fn read() -> AssistantContent {
    call("read_playbook", json!({"path":"playbooks/target"}))
}
fn adapter(f: &Fixture) -> impl frona_harness::Tool + '_ {
    AgentToolAdapter::new(&f.tool, &f.context, "read_playbook", |output| {
        content_result(&output)
    })
    .unwrap()
}

#[tokio::test]
async fn successful_read_reaches_next_inspection_and_accepts_normalized_assignment() {
    let f = fixture().await;
    let (harness, requests) = harness(vec![
        submission(vec![target()]),
        vec![read()],
        submission(vec![target()]),
    ]);
    let inspection = f.inspection.clone();
    let run = harness
        .run("resolve")
        .tool(adapter(&f))
        .structured::<Verdict>()
        .submission_attempts(2);
    let report = run.report();
    let accepted = run
        .inspect(move |value| {
            let inspection = inspection.clone();
            async move { inspection.inspect(value) }
        })
        .await
        .unwrap();
    assert_eq!(accepted[0].path, "playbooks/target");
    assert_eq!(
        accepted[0].existing_path.as_deref(),
        Some("playbooks/target")
    );
    assert_eq!(accepted[0].memory_ids, ["memory-one"]);
    assert!(requests.lock().unwrap()[1].contains("must be found and inspected"));
    assert!(
        f.tool
            .lookup
            .read_paths
            .read()
            .unwrap()
            .contains("playbooks/target")
    );
    assert!(
        report
            .snapshot()
            .feedback
            .iter()
            .any(|feedback| !feedback.issued_in.is_empty())
    );
}

#[tokio::test]
async fn rejected_and_unstarted_reads_never_establish_inspection() {
    for reject in [true, false] {
        let f = fixture().await;
        let (harness, _) = harness(vec![vec![read()], submission(vec![target()])]);
        let inspection = f.inspection.clone();
        let run = harness
            .run("resolve")
            .tool(adapter(&f))
            .structured::<Verdict>()
            .submission_attempts(1);
        let run = if reject {
            run.before_tool(|_| async {
                Ok::<_, ()>(frona_harness::ToolDecision::Reject(
                    "caller declined".into(),
                ))
            })
        } else {
            run.budget(frona_harness::DefaultBudget::new(
                frona_harness::BudgetLimits {
                    tool_calls: Some(0),
                    ..Default::default()
                },
            ))
        };
        let incomplete = run
            .inspect(move |value| {
                let inspection = inspection.clone();
                async move { inspection.inspect(value) }
            })
            .await
            .unwrap_err();
        assert!(incomplete.retained.is_none());
        assert!(f.tool.lookup.read_paths.read().unwrap().is_empty());
        if reject {
            assert!(matches!(
                incomplete.reason,
                frona_harness::ManualError::SubmissionAttemptsExhausted { .. }
            ));
            assert!(!incomplete.report.actions[0].started);
        } else {
            assert!(matches!(
                incomplete.report.outcome,
                Some(frona_harness::RunOutcome::Incomplete {
                    kind: frona_harness::FailureKind::Budget,
                    ..
                })
            ));
        }
    }
}

#[tokio::test]
async fn unknown_or_locally_exhausted_reads_do_not_create_investigation_evidence() {
    for exhausted in [false, true] {
        let f = fixture().await;
        let read = if exhausted {
            f.tool.total.store(0, std::sync::atomic::Ordering::Relaxed);
            read()
        } else {
            call("read_playbook", json!({"path":"playbooks/unknown"}))
        };
        let (harness, requests) = harness(vec![vec![read], submission(vec![target()])]);
        let inspection = f.inspection.clone();
        let incomplete = harness
            .run("resolve")
            .tool(adapter(&f))
            .structured::<Verdict>()
            .submission_attempts(1)
            .inspect(move |value| {
                let inspection = inspection.clone();
                async move { inspection.inspect(value) }
            })
            .await
            .unwrap_err();
        assert!(f.tool.lookup.read_paths.read().unwrap().is_empty());
        assert!(incomplete.retained.is_none());
        assert!(matches!(
            incomplete.reason,
            frona_harness::ManualError::SubmissionAttemptsExhausted { .. }
        ));
        let feedback = if exhausted {
            "Tool budget exhausted"
        } else {
            "Unknown Playbook path"
        };
        assert!(requests.lock().unwrap()[1].contains(feedback));
    }
}

#[tokio::test]
async fn failed_memory_read_does_not_create_investigation_evidence() {
    let mut f = fixture().await;
    // No namespace/database selected: the visible snapshot is readable, but the
    // required memory query fails after entity lookup.
    let disconnected_scope = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    f.tool.memories = Arc::new(crate::db::repo::pkm::PkmRepo::new(disconnected_scope, 8));
    let (harness, requests) = harness(vec![vec![read()], submission(vec![target()])]);
    let inspection = f.inspection.clone();
    let incomplete = harness
        .run("resolve")
        .tool(adapter(&f))
        .structured::<Verdict>()
        .submission_attempts(1)
        .inspect(move |value| {
            let inspection = inspection.clone();
            async move { inspection.inspect(value) }
        })
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        frona_harness::ManualError::SubmissionAttemptsExhausted { .. }
    ));
    assert!(f.tool.lookup.read_paths.read().unwrap().is_empty());
    assert_eq!(
        incomplete.report.actions[0].outcome,
        Some(frona_harness::ActionOutcome::Failed)
    );
    assert!(requests.lock().unwrap()[1].contains("read_playbook"));
    assert!(
        format!("{:?}", incomplete.conversation.messages()).contains("must be found and inspected")
    );
}

#[tokio::test]
async fn duplicate_unknown_and_unsupported_assignments_remain_rejected() {
    let f = fixture().await;
    let (harness, _) = harness(vec![
        vec![read()],
        submission(vec![target(), target()]),
        {
            let mut target = target();
            target["memory_ids"] = json!(["unknown"]);
            submission(vec![target])
        },
        {
            let mut target = target();
            target["merge_from"] = json!(["playbooks/unknown"]);
            submission(vec![target])
        },
    ]);
    let inspection = f.inspection.clone();
    let incomplete = harness
        .run("resolve")
        .tool(adapter(&f))
        .structured::<Verdict>()
        .submission_attempts(3)
        .inspect(move |value| {
            let inspection = inspection.clone();
            async move { inspection.inspect(value) }
        })
        .await
        .unwrap_err();
    let history = format!("{:?}", incomplete.conversation.messages());
    assert!(history.contains("claimed by multiple targets"));
    assert!(history.contains("unknown"));
    assert!(history.contains("is not a Playbook"));
    assert!(incomplete.retained.is_none());
    assert!(matches!(
        incomplete.report.outcome,
        Some(frona_harness::RunOutcome::Incomplete { .. })
    ));
}
