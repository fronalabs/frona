use super::support::*;
use crate::test_support::{harness, tool_output};
use crate::{
    Phase,
    model::Disposition,
    writeback::{PageEditMemoryOp, Writeback, WritebackInput, writeback_diff},
};
use frona_harness::{BudgetLimits, DefaultBudget};
use serde_json::json;
fn input() -> WritebackInput {
    WritebackInput {
        path: "services/postgres".into(),
        head: "port 5432".into(),
        incoming: "port 5433".into(),
        memories: vec![memory("durable-1")],
    }
}

#[tokio::test]
async fn expands_local_ids_and_returns_validated_operations() {
    let (harness, requests) = harness(vec![Ok(tool_output(
        "submit",
        json!({"ops":[
            {"op":"supersede","memory_id":"m1","kind":"Fact","content":"Port is 5433","note":"corrected"},
            {"op":"add","kind":"Preference","content":"Prefer local Postgres"},
            {"op":"outdated","memory_id":"m1"},{"op":"wrong","memory_id":"m1"},
            {"op":"unknown"},{"op":"add","kind":"made-up","content":"bad"},{"op":"add","kind":"Fact","content":" "}
        ]}),
    ))]);
    let out = Phase::run(&Writeback::new(harness), input()).await.unwrap();
    assert_eq!(out.len(), 4);
    assert!(matches!(&out[0],PageEditMemoryOp::Supersede{older_id,..} if older_id=="durable-1"));
    assert!(matches!(
        &out[2],
        PageEditMemoryOp::SetDisposition {
            disposition: Disposition::Outdated,
            ..
        }
    ));
    let requests = requests.lock().unwrap();
    let prompt = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(prompt.contains("m1 | Fact"));
    assert!(!prompt.contains("durable-1"));
}

#[tokio::test]
async fn unknown_model_local_id_fails_instead_of_targeting_another_memory() {
    let (harness, _) = harness(vec![Ok(tool_output(
        "submit",
        json!({"ops":[{"op":"wrong","memory_id":"durable-1"}]}),
    ))]);
    assert!(Writeback::new(harness).run(input()).await.is_err());
}

#[tokio::test]
async fn erroneous_memories_are_not_offered_for_writeback() {
    let (harness, requests) = harness(vec![Ok(tool_output("submit", json!({"ops":[]})))]);
    let mut input = input();
    input.memories[0].disposition = Disposition::Erroneous;
    assert!(Writeback::new(harness).run(input).await.unwrap().is_empty());
    let prompt = serde_json::to_string(&requests.lock().unwrap()[0].messages).unwrap();
    assert!(!prompt.contains("Postgres runs on 5433."));
}

#[tokio::test]
async fn history_only_edits_do_not_call_the_model() {
    let (harness, requests) = harness(vec![]);
    let mut input = input();
    input.head = "article\n## History\nold".into();
    input.incoming = "article\n## History\nnew".into();
    assert!(Writeback::new(harness).run(input).await.unwrap().is_empty());
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn shared_budget_controls_planning() {
    let (harness, requests) = harness(vec![]);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    assert!(
        Writeback::new(harness)
            .run_with(input(), |run| run.budget(&budget))
            .await
            .is_err()
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn copied_diff_ignores_history_and_preserves_human_changes() {
    let d = writeback_diff(
        "# Bob\nWorks at Acme.\n## History\nold",
        "# Bob\nWorks at Globex.\n## History\nDIFFERENT",
    );
    assert!(d.contains("-Works at Acme."));
    assert!(d.contains("+Works at Globex."));
    assert!(!d.contains("DIFFERENT"));
}

#[tokio::test]
async fn invalid_submission_does_not_start_a_domain_repair_loop() {
    let (harness, requests) = harness(vec![Ok(tool_output(
        "submit",
        json!({"ops":"not an operation list"}),
    ))]);
    assert!(Writeback::new(harness).run(input()).await.is_err());
    assert_eq!(requests.lock().unwrap().len(), 1);
}
