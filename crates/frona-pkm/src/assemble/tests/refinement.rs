use super::*;
use frona_harness::{ManualError, inference::InferenceError};
use rig_core::completion::AssistantContent;
use serde_json::{Value, json};
use std::sync::Arc;
struct Fixture {
    gate: GateContext,
    ontology: Ontology,
}
async fn fixture() -> Fixture {
    let graph = GraphSnapshot {
        scope_id: "u".into(),
        ..Default::default()
    };
    let ontology = ontology(graph.clone());
    let gate = GateContext {
        graph,
        prefixes: ontology.prefixes(),
        prompts: Default::default(),
    };
    Fixture { gate, ontology }
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
        fixture.gate.clone(),
        fixture.ontology.clone(),
        proposals(),
        vec![],
        vec![],
    ))
}
fn harness(
    submissions: Vec<Result<AdjudicationResult, InferenceError>>,
) -> frona_harness::Harness<'static> {
    crate::test_support::harness(submissions.into_iter().map(|submission| {
        submission.map(|value| {
            crate::test_support::tool_output("submit", serde_json::to_value(value).unwrap())
        })
    }))
    .0
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
    assert!(matches!(error, Error::Cancelled(_)));
}
