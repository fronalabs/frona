use crate::test_support::{Requests, harness, tool_output};
use crate::{
    Error, Phase, TranscriptEvidenceKind, TranscriptEvidenceSource,
    ingest::{Ingest, IngestBatch, IngestInput, IngestPrompts},
};
use frona_harness::{BudgetLimits, DefaultBudget, ModelOutput};

use serde_json::{Value, json};

fn phase(outputs: Vec<ModelOutput>) -> (Ingest<'static>, Requests) {
    let (harness, requests) = harness(outputs.into_iter().map(Ok));
    (Ingest::new(harness), requests)
}

fn candidate() -> Value {
    json!({"memories": [{"id":"fact-1", "kind":"Fact", "content":"Postgres runs on 5433",
        "entities":["services/postgres"], "sources":[{"message":"m1", "quote":"Postgres runs on 5433", "strength":"explicit"}], "episode":null, "playbook":null}]})
}

fn input() -> IngestInput {
    IngestInput {
        transcript: "[m1] User: Postgres runs on 5433".into(),
        evidence_sources: vec![TranscriptEvidenceSource {
            handle: "m1".into(),
            text: "Postgres runs on 5433".into(),
            kind: TranscriptEvidenceKind::UserMessage {
                message_id: "durable-1".into(),
                chat_id: "chat-1".into(),
            },
        }],
        ..Default::default()
    }
}

async fn invoke<P: Phase<Input = IngestInput, Output = IngestBatch>>(
    phase: &P,
    input: IngestInput,
) -> Result<IngestBatch, Error> {
    phase.run(input).await
}

#[tokio::test]
async fn public_phase_returns_serializable_grounded_batch() {
    let (phase, calls) = phase(vec![tool_output("submit", candidate())]);
    let batch = invoke(&phase, input()).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(batch.memories.len(), 1);
    assert_eq!(batch.memories[0].content, "Postgres runs on 5433");
    assert!(uuid::Uuid::parse_str(&batch.memories[0].id).is_ok());
    assert!(
        matches!(&batch.memories[0].evidence[0].source, crate::model::EvidenceSource::UserMessage { message_id, .. } if message_id == "durable-1")
    );
    let restored: IngestBatch =
        serde_json::from_value(serde_json::to_value(&batch).unwrap()).unwrap();
    assert_eq!(restored.memories[0].id, batch.memories[0].id);
}

#[tokio::test]
async fn empty_transcript_does_not_call_the_model() {
    let (phase, calls) = phase(vec![]);
    assert!(
        phase
            .run(IngestInput::default())
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn invalid_candidate_is_revised_before_acceptance() {
    let mut invalid = candidate();
    invalid["memories"][0]["kind"] = json!("imaginary");
    let (phase, calls) = phase(vec![
        tool_output("submit", invalid),
        tool_output("submit", candidate()),
    ]);
    let batch = phase.run(input()).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(batch.grounding_corrections, 1);
    assert_eq!(batch.memories.len(), 1);
}

#[tokio::test]
async fn previously_rejected_facts_cannot_be_relearned() {
    let (phase, _) = phase(
        (0..10)
            .map(|_| tool_output("submit", candidate()))
            .collect(),
    );
    let mut input = input();
    input.erroneous_contents.insert(
        "services/postgres".into(),
        ["postgres runs on 5433".into()].into(),
    );
    let batch = phase.run(input).await.unwrap();
    assert!(batch.memories.is_empty());
    assert!(batch.grounding_items_dropped > 0);
}

#[tokio::test]
async fn failure_without_a_candidate_is_not_a_successful_empty_batch() {
    let (harness, _) = harness([Err(
        frona_harness::inference::InferenceError::InferenceFailed("transport failed".into()),
    )]);
    let phase = Ingest::new(harness);
    assert!(phase.run(input()).await.is_err());
}

#[tokio::test]
async fn caller_budget_is_checked_before_model_execution() {
    let (phase, calls) = phase(vec![tool_output("submit", candidate())]);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    let result = phase.run_with(input(), |run| run.budget(&budget)).await;
    assert!(result.is_err());
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn cancellation_is_not_salvaged_as_success() {
    let (phase, calls) = phase(vec![tool_output("submit", candidate())]);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let result = phase.run_with(input(), |run| run.cancellation(token)).await;
    assert!(matches!(result, Err(Error::Cancelled(_))));
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn prompt_override_cannot_drop_the_transcript() {
    let (phase, calls) = phase(vec![]);
    let phase = phase.prompts(IngestPrompts {
        input: "Extract memories.".into(),
        ..Default::default()
    });
    assert!(phase.run(input()).await.is_err());
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn recall_tool_reads_the_supplied_snapshot_without_server_state() {
    let (phase, _) = phase(vec![
        tool_output("read_recall_result", json!({"call_id":"T1"})),
        tool_output("submit", candidate()),
    ]);
    let mut input = input();
    input.recall = crate::RecallProjection::new(
        &[crate::evidence::ToolExecution {
            id: "recall-1".into(),
            chat_id: "chat-1".into(),
            message_id: "agent-1".into(),
            turn: 1,
            name: "memory_search".into(),
            arguments: json!({"query":"postgres"}),
            result: "Old recalled content".into(),
            success: true,
            user_response: false,
            created_at: chrono::Utc::now(),
        }],
        |_| false,
    );
    let batch = phase.run(input).await.unwrap();
    assert_eq!(batch.recall_result_lookups, 1);
    assert_eq!(batch.memories.len(), 1);
}

#[tokio::test]
async fn evidence_tool_grounds_agent_claims_and_recovers_durable_provenance() {
    let mut proposed = candidate();
    proposed["memories"][0]["tool_evidence"] = json!([{
        "message":"m1", "evidence_id":"m1:tool1", "quote":"Postgres runs on 5433"
    }]);
    proposed["research_dispositions"] = json!([{
        "message":"m1", "result":"extracted", "claims":[{
            "claim":"Postgres runs on 5433", "result":"extracted", "contribution_ids":["fact-1"]
        }]
    }]);
    let (harness, _) = harness(vec![
        Ok(tool_output(
            "search_tool_evidence",
            json!({"message_id":"m1", "query":"Postgres runs on 5433"}),
        )),
        Ok(tool_output("submit", proposed)),
        // Preserve the original interruption after retention explicitly. An
        // unexpected extra model call must fail instead of becoming test input.
        Err(frona_harness::inference::InferenceError::InferenceFailed(
            "transport failed after retention".into(),
        )),
    ]);
    let phase = Ingest::new(harness);
    let mut input = input();
    input.transcript = "[m1] Agent: Postgres runs on 5433".into();
    input.evidence_sources[0].kind = TranscriptEvidenceKind::AgentMessage {
        message_id: "agent-1".into(),
        agent_id: "assistant".into(),
        chat_id: "chat-1".into(),
    };
    input.recall.evidence = crate::ToolEvidenceProjection::new(
        &[crate::evidence::ToolExecution {
            id: "execution-1".into(),
            chat_id: "chat-1".into(),
            message_id: "agent-1".into(),
            turn: 1,
            name: "inspect_database".into(),
            arguments: json!({}),
            result: "Postgres runs on 5433".into(),
            success: true,
            user_response: false,
            created_at: chrono::Utc::now(),
        }],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let batch = phase.run(input).await.unwrap();
    assert_eq!(batch.memories.len(), 1);
    assert_eq!(batch.agent_evidence_lookup_calls, 1);
    assert_eq!(batch.research_coverage.extracted, 1);
    assert!(batch.memories[0].evidence.iter().any(|evidence| matches!(&evidence.source,
        crate::model::EvidenceSource::ToolResult { tool_call_id, .. } if tool_call_id == "execution-1")));
}

#[tokio::test]
async fn procedural_candidates_reference_the_returned_memory_ids() {
    let mut proposed = candidate();
    proposed["memories"][0]["kind"] = json!("Procedural");
    proposed["memories"][0]["playbook"] = json!("procedure-1");
    proposed["playbooks"] = json!([{
        "id":"procedure-1", "path":"playbooks/postgres", "name":"Postgres procedure", "description":"Configure Postgres"
    }]);
    let (phase, _) = phase(vec![tool_output("submit", proposed)]);
    let batch = phase.run(input()).await.unwrap();
    assert_eq!(batch.playbook_candidates.len(), 1);
    assert_eq!(batch.memories.len(), 1);
    assert_eq!(
        batch.playbook_candidates[0].source_memory_ids,
        [batch.memories[0].id.clone()].into()
    );
    assert!(uuid::Uuid::parse_str(&batch.playbook_candidates[0].id).is_ok());
}

#[tokio::test]
async fn grounded_attributes_are_also_returned_as_fact_memories() {
    let mut proposed = candidate();
    proposed["new_entities"] = json!([{
        "id":"entity-1", "path":"Services/Postgres", "name":"Postgres",
        "sources":[{"message":"m1", "quote":"Postgres", "strength":"explicit"}],
        "candidate_attributes":[{"id":"attribute-1", "key":"port", "value":"5433",
            "sources":[{"message":"m1", "quote":"Postgres runs on 5433", "strength":"explicit"}]}]
    }]);
    let (phase, _) = phase(vec![tool_output("submit", proposed)]);
    let batch = phase.run(input()).await.unwrap();
    assert_eq!(batch.entities.len(), 1);
    assert_eq!(batch.entities[0].path, "services/postgres");
    assert_eq!(batch.entities[0].attributes["port"], "5433");
    let attribute = batch
        .memories
        .iter()
        .find(|memory| memory.content == "port: 5433")
        .unwrap();
    assert_eq!(attribute.paths, ["services/postgres"]);
    assert_eq!(
        attribute.evidence,
        batch.entities[0].attribute_evidence["port"]
    );
}
