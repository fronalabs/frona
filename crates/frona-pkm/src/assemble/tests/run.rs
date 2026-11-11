use super::*;
use crate::test_support::{Requests, output, tool_call};
use crate::{classify::EntityProposal, model::EntityCategory};
use frona_harness::Harness;
use rig_core::completion::AssistantContent;
use serde_json::{Value, json};

pub(super) fn harness(outputs: Vec<Vec<AssistantContent>>) -> (Harness<'static>, Requests) {
    crate::test_support::harness(outputs.into_iter().map(|content| Ok(output(content))))
}
pub(super) fn submit(value: Value) -> Vec<AssistantContent> {
    vec![tool_call("submit", value)]
}
fn input(count: usize) -> AssembleInput {
    let mut proposals = ProposalSet::default();
    let entities = (0..count)
        .map(|index| {
            let path = format!("entity/{index}");
            let mut row = KnowledgeConsolidationEntity::pending(
                "run",
                "u",
                &path,
                EntityCategory::Concept,
                vec![],
                Default::default(),
            );
            row.name = format!("Entity {index}");
            let class = format!("frona:Entity{index}");
            row.kinds = vec![format!("urn:frona:Entity{index}")];
            proposals.stage_input_entity(row.clone());
            proposals.record(
                &path,
                EntityProposal {
                    classes: vec![class.clone()],
                    edits: vec![SchemaEdit::DeclareClass { class }],
                    ..Default::default()
                },
            );
            row
        })
        .collect();
    let graph = GraphSnapshot {
        scope_id: "u".into(),
        entities,
        ..Default::default()
    };
    AssembleInput {
        ontology: ontology(graph.clone()),
        graph,
        proposals,
        banked_adjudication: None,
        resolved_into: Default::default(),
        discarded_paths: Default::default(),
    }
}
fn accepted(count: usize) -> Value {
    json!({"decisions":(0..count).map(|i|json!({"term":format!("frona:Entity{i}"),"decision":"accept_proposal"})).collect::<Vec<_>>()})
}
#[tokio::test]
async fn small_validated_tail_returns_atomic_plan_without_inference() {
    let (h, requests) = harness(vec![]);
    let i = input(1);
    let original = i.proposals.clone();
    let result = Assemble::new(h).run(i).await.unwrap();
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(result.expected_ontology_version, 3);
    assert_eq!(result.stamped, 1);
    assert_eq!(
        result.types,
        vec![("entity/0".into(), vec!["urn:frona:Entity0".into()])]
    );
    assert!(result.ontology_delta.contains("Entity0"));
    assert_eq!(result.materialize.len(), 1);
    assert_eq!(original.by_path.len(), 1);
    assert!(result.banked_adjudication.is_none());
}
#[tokio::test]
async fn full_batch_runs_and_returns_replayable_source_decisions() {
    let (h, requests) = harness(vec![submit(accepted(10))]);
    let result = Assemble::new(h).run(input(10)).await.unwrap();
    assert_eq!(result.stamped, 10);
    assert_eq!(result.banked_adjudication.unwrap().decisions.len(), 10);
    assert_eq!(requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn banked_batch_is_regated_without_calling_model() {
    let (h, requests) = harness(vec![]);
    let mut i = input(10);
    i.banked_adjudication = Some(serde_json::from_value(accepted(10)).unwrap());
    let result = Assemble::new(h).run(i).await.unwrap();
    assert_eq!(result.stamped, 10);
    assert!(requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn failed_model_without_retained_work_never_returns_plan() {
    let (h, _) = crate::test_support::harness([Err(
        frona_harness::inference::InferenceError::InferenceFailed("transport failed".into()),
    )]);
    assert!(Assemble::new(h).run(input(10)).await.is_err());
}
#[tokio::test]
async fn merge_and_discard_outputs_include_sources_aliases_and_canonical_targets() {
    let (h, _) = harness(vec![]);
    let mut i = input(1);
    let mut source = i.graph.entities[0].clone();
    source.path = "losing".into();
    source.name = "Old name".into();
    source.source_memory_ids = vec!["old-memory".into()];
    i.graph.entities.push(source);
    i.resolved_into.insert("losing".into(), "entity/0".into());
    i.discarded_paths.insert("empty".into());
    let result = Assemble::new(h).run(i).await.unwrap();
    assert_eq!(
        result.coalesced_sources,
        vec![("losing".into(), "entity/0".into(), "old-memory".into())]
    );
    assert_eq!(
        result.coalesced_aliases,
        vec![("entity/0".into(), vec!["Old name".into()])]
    );
    assert!(result.working_outcomes.contains(&("empty".into(), None)));
    assert!(
        result
            .working_outcomes
            .contains(&("losing".into(), Some("entity/0".into())))
    );
}
#[tokio::test]
async fn cyclic_merge_plan_is_rejected_before_model() {
    let (h, requests) = harness(vec![]);
    let mut i = input(10);
    i.resolved_into.insert("a".into(), "b".into());
    i.resolved_into.insert("b".into(), "a".into());
    assert!(Assemble::new(h).run(i).await.is_err());
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_tail_never_returns_commit_plan() {
    let (h, requests) = harness(vec![]);
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        Assemble::new(h)
            .cancellation(cancellation)
            .run(input(1))
            .await,
        Err(Error::Cancelled(_))
    ));
    assert!(requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn discarded_rows_do_not_introduce_schema_proposals() {
    let (h, _) = harness(vec![]);
    let mut i = input(1);
    let mut discarded = i.graph.entities[0].clone();
    discarded.path = "discarded".into();
    discarded.lifecycle = crate::model::ConsolidationEntityLifecycle::Discarded;
    discarded.attributes = json!({"frona:discardedProperty":"must not enter schema"});
    i.graph.entities.push(discarded);
    let result = Assemble::new(h).run(i).await.unwrap();
    assert!(!result.ontology_delta.contains("discardedProperty"));
}

#[derive(Default)]
struct Checkpoint {
    recorded: std::sync::Mutex<Vec<AdjudicationResult>>,
    fail: bool,
}
#[async_trait::async_trait]
impl AdjudicationCheckpoint for Checkpoint {
    async fn record(&self, result: &AdjudicationResult) -> Result<(), Error> {
        if self.fail {
            return Err(Error::Internal("checkpoint unavailable".into()));
        }
        self.recorded.lock().unwrap().push(result.clone());
        Ok(())
    }
}

#[tokio::test]
async fn accepted_batch_is_checkpointed_before_plan_is_returned() {
    let (h, _) = harness(vec![submit(accepted(10))]);
    let checkpoint = Checkpoint::default();
    let output = Assemble::new(h)
        .run_with_checkpoint(input(10), |run| run, &checkpoint)
        .await
        .unwrap();
    assert_eq!(output.stamped, 10);
    assert_eq!(checkpoint.recorded.lock().unwrap()[0].decisions.len(), 10);
}

#[tokio::test]
async fn checkpoint_failure_prevents_publication_plan() {
    let (h, requests) = harness(vec![submit(accepted(10))]);
    let checkpoint = Checkpoint {
        fail: true,
        ..Default::default()
    };
    let result = Assemble::new(h)
        .run_with_checkpoint(input(10), |run| run, &checkpoint)
        .await;
    assert!(matches!(result, Err(Error::Internal(message)) if message == "checkpoint unavailable"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(checkpoint.recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_batch_without_retained_candidate_is_not_checkpointed() {
    let (h, _) = crate::test_support::harness([Err(
        frona_harness::inference::InferenceError::InferenceFailed("transport failed".into()),
    )]);
    let checkpoint = Checkpoint::default();
    assert!(
        Assemble::new(h)
            .run_with_checkpoint(input(10), |run| run, &checkpoint)
            .await
            .is_err()
    );
    assert!(checkpoint.recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn retained_source_is_checkpointed_but_only_regated_edits_enter_plan() {
    let mut source = accepted(10);
    source["decisions"][9]["term"] = json!("frona:NotOffered");
    let (h, requests) = harness(vec![submit(source)]);
    let checkpoint = Checkpoint::default();
    let output = Assemble::new(h)
        .submission_attempts(1)
        .run_with_checkpoint(input(10), |run| run, &checkpoint)
        .await
        .unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    let recorded = checkpoint.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert!(
        recorded[0]
            .decisions
            .iter()
            .any(|decision| decision.term == "frona:NotOffered")
    );
    assert!(!output.ontology_delta.contains("NotOffered"));
    assert!(
        output
            .types
            .iter()
            .all(|(_, kinds)| kinds.iter().all(|kind| !kind.contains("NotOffered")))
    );
}
