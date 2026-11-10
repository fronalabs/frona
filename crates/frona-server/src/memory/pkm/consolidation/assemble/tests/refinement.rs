use super::*;
use crate::agent::prompt::PromptLoader;
use crate::memory::pkm::{
    consolidation::{
        ConsolidationScope, context::ConsolidationContext, inference::ConsolidationInference,
    },
    ontology::Roots,
    storage::PkmStorage,
};
use frona_harness::{
    Capabilities, Handle, ManualError, ModelAdapter, ModelOutput, ModelRequest, UsageReport,
    config::ProviderModel,
    inference::{InferenceError, ModelConfig, ModelGroup, provider::StreamToken},
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Fixture {
    _app: crate::app_state_fixture::Fixture,
    prompts: tempfile::TempDir,
    consolidator: Consolidator,
}
async fn fixture() -> Fixture {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let app = crate::app_state_fixture::build(&db).await;
    let repo = Arc::new(crate::db::repo::pkm::PkmRepo::new(db.clone(), 10));
    let storage = PkmStorage::new(app.state.storage_service.clone());
    let scope = ConsolidationScope {
        user_id: "u".into(),
        user_name: "Owner".into(),
        agent_id: "unused".into(),
        chat_id: None,
        vault: storage
            .vault_scope(crate::handle!("owner"), "memory")
            .unwrap(),
        temporal_sources: vec![],
        evidence_sources: vec![],
        recall: Default::default(),
        timezone: "UTC".into(),
    };
    let prompts = tempfile::tempdir().unwrap();
    let target = prompts.path().join("pkm/assemble");
    std::fs::create_dir_all(&target).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../resources/prompts/pkm/assemble");
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
    let model = ModelConfig {
        catalog_provider: String::new(),
        provider_handle: Handle::const_validated("unused"),
        model_id: "unused".into(),
        provider: ProviderModel::Generic,
        request_settings: Default::default(),
    };
    let group = ModelGroup {
        name: "unused".into(),
        main: model,
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
        PromptLoader::new(prompts.path()),
        "u".into(),
    );
    let context = Arc::new(ConsolidationContext::detached(
        scope,
        repo.clone(),
        storage,
        llm,
    ));
    let ontologies =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology");
    let ontology = OntologyManager::new(
        Roots {
            release: ontologies.join("standard"),
            user: ontologies.join("no-user"),
        },
        repo,
    );
    let consolidator = Consolidator::new(
        context,
        Default::default(),
        ontology,
        Arc::new(crate::db::repo::tool_calls::SurrealToolCallRepo::new(
            db.clone(),
        )),
        crate::db::repo::messages::SurrealMessageRepo::new(db),
        app.state.user_service.clone(),
    );
    Fixture {
        _app: app,
        prompts,
        consolidator,
    }
}
fn proposals() -> Vec<Proposal> {
    ["A", "B", "C"]
        .into_iter()
        .map(|name| {
            let term = format!("frona:{name}");
            let mut proposed_edits = vec![
                SchemaEdit::DeclareClass {
                    class: term.clone(),
                },
                SchemaEdit::SubClassOf {
                    sub: term.clone(),
                    sup: "schema:Thing".into(),
                },
            ];
            if name != "C" {
                proposed_edits.push(SchemaEdit::SubClassOf {
                    sub: term.clone(),
                    sup: "schema:Person".into(),
                });
            }
            Proposal {
                term,
                kind: ProposalKind::Class,
                usage_entities: 0,
                usage_links: 0,
                description: name.into(),
                proposed_edits,
            }
        })
        .collect()
}
fn submission(a: Value, b: Value, c: Value) -> AdjudicationResult {
    serde_json::from_value(json!({"decisions": [
        merge("frona:A", a), merge("frona:B", b), merge("frona:C", c)
    ]}))
    .unwrap()
}
fn merge(term: &str, mut decision: Value) -> Value {
    decision["term"] = term.into();
    decision
}
fn accept() -> Value {
    json!({"decision":"accept_proposal"})
}
fn align() -> Value {
    json!({"decision":"align", "standard":"schema:Person"})
}
fn invalid() -> Value {
    json!({"decision":"declare", "parent":"schema:Organization"})
}
fn partial() -> AdjudicationResult {
    submission(accept(), accept(), invalid())
}
fn inspection(fixture: &Fixture) -> Arc<AdjudicationInspection> {
    Arc::new(AdjudicationInspection::new(
        fixture.consolidator.clone(),
        fixture.consolidator.ontology.clone(),
        proposals(),
        vec![],
        vec![],
    ))
}
struct Script(Mutex<VecDeque<Result<AdjudicationResult, InferenceError>>>);
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
        _: ModelRequest,
        _: Option<tokio::sync::mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        let submission = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call")?;
        Ok(ModelOutput::new(
            vec![AssistantContent::ToolCall(ToolCall::from_wire(
                "s",
                ToolFunction::new(
                    "submit".try_into().unwrap(),
                    serde_json::to_value(submission).unwrap(),
                ),
            ))],
            UsageReport::Unavailable,
        ))
    }
}
fn harness(
    submissions: Vec<Result<AdjudicationResult, InferenceError>>,
) -> frona_harness::Harness<'static> {
    frona_harness::Harness::new(
        frona_harness::Model::from_adapter(
            "fixture",
            "adjudication",
            Script(Mutex::new(submissions.into())),
        )
        .unwrap(),
    )
}
async fn inspect_sequence(
    fixture: &Fixture,
    submissions: Vec<AdjudicationResult>,
) -> Incomplete<AdjudicationCandidate> {
    let maximum = submissions.len();
    let harness = harness(submissions.into_iter().map(Ok).collect());
    let inspection = inspection(fixture);
    harness
        .run("adjudicate")
        .structured::<AdjudicationResult>()
        .submission_attempts(maximum)
        .inspect(move |submission| {
            let inspection = inspection.clone();
            async move { inspection.inspect(submission).await }
        })
        .await
        .expect_err("the preferred partial candidate remains unfinished")
}
fn assert_retained(incomplete: &Incomplete<AdjudicationCandidate>, expected: &AdjudicationResult) {
    let retained = incomplete.retained.as_ref().unwrap();
    assert_eq!(retained.plan.edits.len(), 6);
    assert_eq!(
        serde_json::to_value(&retained.submission).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert!(matches!(
        incomplete.report.outcome,
        Some(RunOutcome::Incomplete { .. })
    ));
    assert!(
        incomplete.conversation.responses().iter().any(|response| {
            response.output().content.iter().any(|content| {
                matches!(content,
            AssistantContent::ToolCall(call)
                if serde_json::Value::Object(call.function.arguments.clone()) == serde_json::to_value(expected).unwrap())
            })
        }),
        "the original retained submission remains in response history"
    );
}

#[tokio::test]
async fn worse_complete_revision_abandons_without_erasing_preferred_plan_and_source() {
    let fixture = fixture().await;
    let preferred = partial();
    let incomplete = inspect_sequence(
        &fixture,
        vec![preferred.clone(), submission(align(), align(), accept())],
    )
    .await;
    assert!(matches!(incomplete.reason, ManualError::Abandoned(_)));
    assert_retained(&incomplete, &preferred);
}

#[tokio::test]
async fn worse_and_unwritable_revisions_preserve_retention_until_submission_cap() {
    let fixture = fixture().await;
    let preferred = partial();
    let incomplete = inspect_sequence(
        &fixture,
        vec![
            preferred.clone(),
            submission(align(), invalid(), invalid()),
            submission(
                json!({"decision":"align","standard":"unknown:Invalid"}),
                accept(),
                accept(),
            ),
        ],
    )
    .await;
    assert!(matches!(
        incomplete.reason,
        ManualError::SubmissionAttemptsExhausted { maximum: 3 }
    ));
    assert_retained(&incomplete, &preferred);
}

#[tokio::test]
async fn equal_rank_revision_replaces_retention() {
    let fixture = fixture().await;
    let mut latest = partial();
    latest.decisions[2].decision = Decision::Declare {
        parent: Some("schema:SoftwareApplication".into()),
        datatype: None,
        domain: None,
        range: None,
        inverse: None,
        characteristics: vec![],
    };
    let incomplete = inspect_sequence(&fixture, vec![partial(), latest.clone()]).await;
    assert_retained(&incomplete, &latest);
}

#[tokio::test]
async fn inspector_failure_keeps_prior_plan_and_source_with_failure_reason() {
    let fixture = fixture().await;
    let inspection = inspection(&fixture);
    let harness = harness(vec![Ok(partial()), Ok(partial())]);
    let reject_file = fixture.prompts.path().join("pkm/assemble/reject.md");
    let mut calls = 0;
    let incomplete = harness
        .run("adjudicate")
        .structured::<AdjudicationResult>()
        .inspect(async |submission| {
            calls += 1;
            if calls == 2 {
                std::fs::remove_file(&reject_file).unwrap();
            }
            inspection.inspect(submission).await
        })
        .await
        .expect_err("missing feedback template is an inspector error");
    assert_eq!(calls, 2);
    assert!(matches!(incomplete.reason, ManualError::Inspector(_)));
    assert_retained(&incomplete, &partial());
}

#[tokio::test]
async fn retained_source_is_regated_against_live_ontology_before_salvage() {
    let fixture = fixture().await;
    let incomplete = inspect_sequence(&fixture, vec![partial()]).await;
    let retained = retained_for_salvage(incomplete).unwrap();
    fixture
        .consolidator
        .ontology
        .commit(
            "u",
            &[SchemaEdit::SubClassOf {
                sub: "frona:A".into(),
                sup: "schema:Organization".into(),
            }],
        )
        .await
        .unwrap();
    let (regated, rejected) = fixture
        .consolidator
        .gate_submission(
            &fixture.consolidator.ontology,
            &proposals(),
            &retained.submission,
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(!rejected.is_empty());
    assert!(regated.edits.len() < retained.plan.edits.len());
    assert!(
        !regated
            .edits
            .iter()
            .any(|edit| matches!(edit, SchemaEdit::DeclareClass { class } if class == "frona:A"))
    );
    fixture
        .consolidator
        .validate(&fixture.consolidator.ontology, regated, &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_never_enters_retained_salvage() {
    let fixture = fixture().await;
    let inspection = inspection(&fixture);
    let harness = harness(vec![
        Ok(partial()),
        Err(InferenceError::Cancelled("stop".into())),
    ]);
    let incomplete = harness
        .run("adjudicate")
        .structured::<AdjudicationResult>()
        .inspect(move |submission| {
            let inspection = inspection.clone();
            async move { inspection.inspect(submission).await }
        })
        .await
        .expect_err("cancelled");
    assert_retained(&incomplete, &partial());
    let error = retained_for_salvage(incomplete).unwrap_err();
    assert!(
        matches!(error, AppError::Inference(error) if matches!(*error, InferenceError::Cancelled(_)))
    );
}
