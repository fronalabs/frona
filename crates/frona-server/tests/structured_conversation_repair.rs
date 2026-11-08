//! SDK regressions migrated from the removed StructuredConversation engine.
//! Malformed submissions get paired feedback, mixed siblings keep their results,
//! and prose gets a plain nudge. Local exploration and submission caps are distinct.
mod helpers;

use frona_harness::{
    Decision, Harness, ManualError, Model, Submission, ToolOutput,
    inference::{ModelGroup, provider::SUBMIT_TOOL_NAME},
    tool,
};
use helpers::{MockModelProvider, MockResponse, test_model_group, test_providers};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct Classification {
    classes: Vec<String>,
    #[serde(default)]
    relations: Vec<String>,
}
fn submit(id: &str, args: Value) -> MockResponse {
    MockResponse::ToolCalls(vec![(id.into(), SUBMIT_TOOL_NAME.into(), args)])
}
fn harness(responses: Vec<MockResponse>) -> (Harness<'static>, Arc<MockModelProvider>) {
    let provider = Arc::new(MockModelProvider::new(responses));
    let group = ModelGroup {
        providers: test_providers("mock", provider.clone()),
        ..test_model_group()
    };
    (Harness::new(Model::from_group(group)), provider)
}
fn answer_text<'a>(history: &'a [rig_core::completion::Message], id: &str) -> Option<&'a str> {
    history.iter().find_map(|message| {
        let rig_core::completion::Message::User { content } = message else {
            return None;
        };
        content.iter().find_map(|item| {
            let rig_core::completion::message::UserContent::ToolResult(result) = item else {
                return None;
            };
            (result.call.as_str() == id)
                .then(|| result.content.iter().find_map(|item| item.as_text()))
                .flatten()
        })
    })
}

#[tokio::test]
async fn a_malformed_submission_is_returned_to_the_model_and_corrected() {
    let (sdk, provider) = harness(vec![
        submit("call-1", json!({"relations":["works for"]})),
        submit("call-2", json!({"classes":["schema:Person"]})),
    ]);
    let got = sdk
        .run("classify this")
        .structured::<Classification>()
        .await
        .unwrap();
    assert_eq!(got.classes, ["schema:Person"]);
    assert!(got.relations.is_empty());
    assert_eq!(provider.calls(), 2);
    assert!(answer_text(&provider.last_history(), "call-1").is_some());
}

#[tokio::test]
async fn a_json_encoded_collection_is_repaired_before_submission() {
    let (sdk, provider) = harness(vec![submit(
        "call-1",
        json!({
            "classes":["schema:Person"], "relations":"[\"works for\"]"
        }),
    )]);
    let got = sdk
        .run("classify this")
        .structured::<Classification>()
        .await
        .unwrap();
    assert_eq!(got.classes, ["schema:Person"]);
    assert_eq!(got.relations, ["works for"]);
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn an_unrepairable_field_gets_schema_specific_feedback() {
    let (sdk, provider) = harness(vec![
        submit(
            "call-1",
            json!({"classes":["schema:Person"], "relations":"not json"}),
        ),
        submit(
            "call-2",
            json!({"classes":["schema:Person"], "relations":["works for"]}),
        ),
    ]);
    sdk.run("classify this")
        .structured::<Classification>()
        .await
        .unwrap();
    let history = provider.last_history();
    let feedback = answer_text(&history, "call-1").expect("paired repair");
    assert!(feedback.contains("/relations"), "{feedback}");
    assert!(feedback.contains("array"), "{feedback}");
    assert!(!feedback.contains("TOP level"), "{feedback}");
}

#[tokio::test]
async fn the_correction_answers_the_submit_call_rather_than_arriving_as_a_user_message() {
    let (sdk, provider) = harness(vec![
        submit("call-1", json!({})),
        submit("call-2", json!({"classes":[]})),
    ]);
    sdk.run("classify this")
        .structured::<Classification>()
        .await
        .unwrap();
    assert!(answer_text(&provider.last_history(), "call-1").is_some());
}

#[tokio::test]
async fn a_tool_call_batched_with_a_bad_submit_still_gets_its_result() {
    let (sdk, provider) = harness(vec![
        MockResponse::ToolCalls(vec![
            ("call-bad".into(), "submit".into(), json!({})),
            ("call-tool".into(), "nonexistent_tool".into(), json!({})),
        ]),
        submit("call-ok", json!({"classes":["schema:Person"]})),
    ]);
    sdk.run("classify this")
        .structured::<Classification>()
        .await
        .unwrap();
    let history = provider.last_history();
    assert!(answer_text(&history, "call-bad").is_some());
    assert!(answer_text(&history, "call-tool").is_some());
}

#[tokio::test]
async fn prose_instead_of_a_submission_is_asked_for_one_rather_than_hung_up_on() {
    let (sdk, provider) = harness(vec![
        MockResponse::Text("Looks like a person".into()),
        submit("call-1", json!({"classes":["schema:Person"]})),
    ]);
    let got = sdk
        .run("classify this")
        .structured::<Classification>()
        .await
        .unwrap();
    assert_eq!(got.classes, ["schema:Person"]);
    assert_eq!(provider.calls(), 2);
}

#[tokio::test]
async fn the_nudge_for_prose_is_not_addressed_to_a_tool_call() {
    let (sdk, provider) = harness(vec![
        MockResponse::Text("no tools thanks".into()),
        submit("call-1", json!({"classes":[]})),
    ]);
    sdk.run("classify this")
        .structured::<Classification>()
        .await
        .unwrap();
    let history = provider.last_history();
    assert!(
        !history
            .iter()
            .any(|m| format!("{m:?}").contains("ToolResult"))
    );
    assert!(history.iter().any(|m| format!("{m:?}").contains("submit")));
}

#[tokio::test]
async fn prose_returns_one_missing_submission_per_request() {
    let (sdk, provider) = harness(
        (0..3)
            .map(|i| MockResponse::Text(format!("thinking {i}")))
            .collect(),
    );
    let mut session = sdk
        .run("classify this")
        .structured::<Classification>()
        .exploration_turns(0)
        .submission_attempts(3)
        .manual();
    for _ in 0..3 {
        assert!(session.call_model().await.unwrap().actions.is_empty());
        session.repair_missing_submission().unwrap();
    }
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::SubmissionAttemptsExhausted { maximum: 3 })
    ));
    assert_eq!(provider.calls(), 3);
    assert_eq!(session.report().snapshot().requests.len(), 3);
}

#[tokio::test]
async fn malformed_submissions_do_not_consume_the_tool_turn_limit() {
    let (sdk, provider) = harness(
        (0..3)
            .map(|i| submit(&format!("call-{i}"), json!({})))
            .collect(),
    );
    let mut session = sdk
        .run("classify this")
        .structured::<Classification>()
        .exploration_turns(0)
        .submission_attempts(3)
        .manual();
    for _ in 0..3 {
        let turn = session.call_model().await.unwrap();
        assert!(matches!(
            session.submission(&turn.actions[0]).unwrap(),
            Submission::Repaired { .. }
        ));
    }
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::SubmissionAttemptsExhausted { maximum: 3 })
    ));
    assert_eq!(provider.calls(), 3);
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Empty {}

#[tokio::test]
async fn the_last_tool_turn_can_be_followed_by_a_submission() {
    let (sdk, provider) = harness(vec![
        MockResponse::ToolCalls(vec![("call-tool".into(), "lookup".into(), json!({}))]),
        submit("call-2", json!({"classes":["schema:Person"]})),
    ]);
    let got = sdk
        .run("classify this")
        .tool(
            tool("lookup", "Find evidence", |_: Empty| async {
                Ok(ToolOutput::text("found"))
            })
            .unwrap(),
        )
        .exploration_turns(1)
        .structured::<Classification>()
        .submission_attempts(1)
        .await
        .unwrap();
    assert_eq!(got.classes, ["schema:Person"]);
    assert_eq!(provider.calls(), 2);
    let offered = provider.tool_histories();
    assert!(offered[0].iter().any(|t| t.name == "lookup"));
    assert_eq!(
        offered[1]
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        [SUBMIT_TOOL_NAME]
    );
    assert!(answer_text(&provider.last_history(), "call-tool").is_some());
}

#[tokio::test]
async fn a_rejected_submission_is_answered_before_the_next_attempt() {
    let (sdk, provider) = harness(vec![
        submit("call-1", json!({"classes":["schema:Thing"]})),
        submit("call-2", json!({"classes":["schema:Person"]})),
    ]);
    let result = sdk
        .run("classify this")
        .structured::<Classification>()
        .inspect(async |value| {
            Ok::<_, String>(if value.classes == ["schema:Thing"] {
                Decision::revise("schema:Thing is too broad")
            } else {
                Decision::accept(value)
            })
        })
        .await
        .unwrap();
    assert_eq!(result.classes, ["schema:Person"]);
    assert_eq!(
        answer_text(&provider.last_history(), "call-1"),
        Some("schema:Thing is too broad")
    );
}

#[tokio::test]
async fn rejection_without_a_pending_submission_is_refused() {
    let (sdk, provider) = harness(vec![]);
    let mut empty = sdk.run("empty").structured::<Classification>().manual();
    assert!(empty.pending_actions().unwrap().actions.is_empty());
    // There is no fabricated handle for "the last submission". Even a real handle
    // from another dialogue cannot reject a nonexistent local submission.
    let (other, _) = harness(vec![submit("call-1", json!({"classes":[]}))]);
    let mut other = other.run("other").structured::<Classification>().manual();
    let action = other.call_model().await.unwrap().actions.pop().unwrap();
    assert!(empty.revise(action, "nothing to reject").is_err());
    assert_eq!(provider.calls(), 0);
}
