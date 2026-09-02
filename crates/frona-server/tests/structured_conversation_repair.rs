//! A `submit` that does not match the schema is a **round**, not the end of the
//! dialogue.
//!
//! The model committed to an answer and got a field wrong. Hanging up throws away a
//! multi-turn conversation - the most expensive thing the consolidation pipeline does
//! per page - and teaches the model nothing, so the next pass re-runs it and gets the
//! same malformed answer. Returning the schema error as the submit call's result lets it
//! correct itself in the conversation it is already in.
//!
//! Two properties are pinned here, and the second is the one that is easy to get wrong:
//! the repair must be delivered as a `tool_result` answering the submit call, and it
//! must not skip any exploration calls the model batched alongside it. An assistant turn
//! containing `tool_use` blocks is only a valid request once *every* one of them has a
//! result, so a repair path that jumps over the siblings produces a conversation the
//! provider rejects outright.

mod helpers;

use std::sync::Arc;

use helpers::{
    MockInternalTool, MockModelProvider, MockResponse, mock_context, test_model_group,
    test_registry_with_group, test_usage_ctx, test_usage_service,
};
use serde::Deserialize;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

use frona::inference::provider::SUBMIT_TOOL_NAME;
use frona::inference::{AnswerAttempt, StructuredConversation};
use frona::tool::registry::AgentToolRegistry;

/// Mirrors the shape that failed in production: a required field alongside one with a
/// `serde` default, so a submission carrying only the optional half parses as a JSON
/// object and fails on the missing field - not on the type.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct Classification {
    classes: Vec<String>,
    #[serde(default)]
    relations: Vec<String>,
}

async fn test_db() -> Surreal<Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();
    db
}

fn submit(id: &str, args: serde_json::Value) -> MockResponse {
    MockResponse::ToolCalls(vec![(id.into(), SUBMIT_TOOL_NAME.into(), args)])
}

/// Is there a **tool result** for `id` in the history - as opposed to the `tool_use`
/// bearing that id, which is present either way once the assistant turn is appended?
/// Both have to be on the same message for it to count.
fn answered(history: &[rig_core::completion::Message], id: &str) -> bool {
    history.iter().any(|m| {
        let rendered = format!("{m:?}");
        rendered.contains("ToolResult") && rendered.contains(id)
    })
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

/// The whole point: a malformed submission is answered, not fatal.
#[tokio::test]
async fn a_malformed_submission_is_returned_to_the_model_and_corrected() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        // Round 1 - `classes` omitted. `relations` alone deserializes fine, which is
        // exactly why this reaches serde as "missing field" rather than a type error.
        submit("call-1", serde_json::json!({ "relations": ["works for"] })),
        submit(
            "call-2",
            serde_json::json!({ "classes": ["schema:Person"] }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        8,
    );

    assert!(matches!(
        convo
            .next_attempt()
            .await
            .expect("malformed attempt returned"),
        AnswerAttempt::InvalidSubmission
    ));
    let AnswerAttempt::Submitted(got) = convo.next_attempt().await.expect("corrected attempt")
    else {
        panic!("the second attempt must be a valid submission");
    };
    assert_eq!(got.classes, ["schema:Person"]);
    assert!(
        got.relations.is_empty(),
        "the defaulted field is still defaulted"
    );
    assert_eq!(
        provider.calls(),
        2,
        "one repair round, not a fresh conversation"
    );
}

/// Some models encode a nested collection twice, leaving valid JSON inside a string. The
/// submission can be recovered without another model request when the decoded value satisfies
/// the advertised schema and the complete payload still deserializes as the requested type.
#[tokio::test]
async fn a_json_encoded_collection_is_repaired_before_submission() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![submit(
        "call-1",
        serde_json::json!({
            "classes": ["schema:Person"],
            "relations": "[\"works for\"]",
        }),
    )]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        0,
    );

    let AnswerAttempt::Submitted(got) = convo.next_attempt().await.expect("repaired submission")
    else {
        panic!("valid JSON text should be repaired before returning the answer attempt");
    };
    assert_eq!(got.classes, ["schema:Person"]);
    assert_eq!(got.relations, ["works for"]);
    assert_eq!(
        provider.calls(),
        1,
        "repair does not spend another model call"
    );
}

/// When a string cannot be repaired into the field's advertised type, the model needs the
/// schema location and expected type. The fields are already at the top level.
#[tokio::test]
async fn an_unrepairable_field_gets_schema_specific_feedback() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        submit(
            "call-1",
            serde_json::json!({
                "classes": ["schema:Person"],
                "relations": "not json",
            }),
        ),
        submit(
            "call-2",
            serde_json::json!({
                "classes": ["schema:Person"],
                "relations": ["works for"],
            }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        0,
    );

    assert!(matches!(
        convo.next_attempt().await.expect("unrepairable attempt"),
        AnswerAttempt::InvalidSubmission
    ));
    assert!(matches!(
        convo.next_attempt().await.expect("corrected attempt"),
        AnswerAttempt::Submitted(_)
    ));

    let history = provider.last_history();
    let feedback = answer_text(&history, "call-1").expect("failed submit has feedback");
    assert!(
        feedback.contains("/relations"),
        "feedback identifies the malformed field: {feedback}"
    );
    assert!(
        feedback.contains("array"),
        "feedback names the expected schema type: {feedback}"
    );
    assert!(
        !feedback.contains("TOP level"),
        "top-level placement was already correct: {feedback}"
    );
}

/// The correction has to be a `tool_result` answering the submit call. A plain user
/// message would leave the `tool_use` in the assistant turn unanswered, and the provider
/// rejects the follow-up request rather than replying to it.
#[tokio::test]
async fn the_correction_answers_the_submit_call_rather_than_arriving_as_a_user_message() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        submit("call-1", serde_json::json!({})),
        submit(
            "call-2",
            serde_json::json!({ "classes": ["schema:Person"] }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        8,
    );
    assert!(matches!(
        convo
            .next_attempt()
            .await
            .expect("malformed attempt returned"),
        AnswerAttempt::InvalidSubmission
    ));
    assert!(matches!(
        convo.next_attempt().await.expect("corrected"),
        AnswerAttempt::Submitted(_)
    ));

    let sent = provider.last_history();
    assert!(
        answered(&sent, "call-1"),
        "the failed submit was answered as a tool result: {sent:#?}"
    );
}

/// A model may batch `submit` with an exploration call in one turn. Both are `tool_use`
/// blocks in the same assistant message, so both need results - the repair path must run
/// the siblings, not jump over them.
#[tokio::test]
async fn a_tool_call_batched_with_a_bad_submit_still_gets_its_result() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![
            (
                "call-bad".into(),
                SUBMIT_TOOL_NAME.into(),
                serde_json::json!({}),
            ),
            (
                "call-tool".into(),
                "nonexistent_tool".into(),
                serde_json::json!({}),
            ),
        ]),
        submit(
            "call-ok",
            serde_json::json!({ "classes": ["schema:Person"] }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        8,
    );
    assert!(matches!(
        convo
            .next_attempt()
            .await
            .expect("malformed attempt returned"),
        AnswerAttempt::InvalidSubmission
    ));
    assert!(matches!(
        convo.next_attempt().await.expect("corrected"),
        AnswerAttempt::Submitted(_)
    ));

    // Both ids ANSWERED - not merely present. Every id appears in the history anyway as
    // the `tool_use` in the assistant turn, so a substring search over the whole
    // conversation proves nothing; the result has to be found on a message that is
    // actually a tool result.
    let sent = provider.last_history();
    assert!(
        answered(&sent, "call-bad"),
        "the failed submit was answered: {sent:#?}"
    );
    assert!(
        answered(&sent, "call-tool"),
        "a tool call batched alongside the bad submit must still get a result, or the \
         next request carries a dangling tool_use: {sent:#?}"
    );
}

/// The other way to not submit: reply with prose. Same treatment, same reason - the
/// exploration turns are already paid for, and nothing in this dialogue is persisted, so
/// hanging up loses all of it over a model that answered the wrong question.
#[tokio::test]
async fn prose_instead_of_a_submission_is_asked_for_one_rather_than_hung_up_on() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("Sure! This entity looks like a person to me.".into()),
        submit(
            "call-1",
            serde_json::json!({ "classes": ["schema:Person"] }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        8,
    );

    assert!(matches!(
        convo
            .next_attempt()
            .await
            .expect("missing attempt returned"),
        AnswerAttempt::MissingSubmission
    ));
    let AnswerAttempt::Submitted(got) = convo.next_attempt().await.expect("corrected attempt")
    else {
        panic!("the second attempt must be a valid submission");
    };
    assert_eq!(got.classes, ["schema:Person"]);
    assert_eq!(provider.calls(), 2, "one nudge, not a fresh conversation");
}

/// The nudge is a plain user message, not a tool result: a text turn leaves no `tool_use`
/// outstanding, so there is no id to answer and inventing one would malform the request.
#[tokio::test]
async fn the_nudge_for_prose_is_not_addressed_to_a_tool_call() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::Text("no tools for me thanks".into()),
        submit(
            "call-1",
            serde_json::json!({ "classes": ["schema:Person"] }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        8,
    );
    assert!(matches!(
        convo
            .next_attempt()
            .await
            .expect("missing attempt returned"),
        AnswerAttempt::MissingSubmission
    ));
    assert!(matches!(
        convo.next_attempt().await.expect("corrected"),
        AnswerAttempt::Submitted(_)
    ));

    let sent = provider.last_history();
    assert!(
        !sent.iter().any(|m| format!("{m:?}").contains("ToolResult")),
        "nothing to answer, so no tool result was fabricated: {sent:#?}"
    );
    assert!(
        sent.iter().any(|m| format!("{m:?}").contains("submit")),
        "and the nudge names the tool to call: {sent:#?}"
    );
}

/// Each prose response is one visible answer attempt. It does not consume the hidden
/// exploration allowance or retry inside `next_attempt`.
#[tokio::test]
async fn prose_returns_one_missing_submission_per_request() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(
        (0..3)
            .map(|i| MockResponse::Text(format!("thinking aloud {i}")))
            .collect(),
    ));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        0,
    );

    for _ in 0..3 {
        assert!(matches!(
            convo
                .next_attempt()
                .await
                .expect("missing attempt returned"),
            AnswerAttempt::MissingSubmission
        ));
    }
    assert_eq!(
        provider.calls(),
        3,
        "one provider request per answer attempt"
    );
    assert_eq!(convo.requests_used(), 3);
}

/// A malformed submit is also one visible answer attempt. The caller owns the number of
/// submission attempts it permits.
#[tokio::test]
async fn malformed_submissions_do_not_consume_the_tool_turn_limit() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(
        (0..3)
            .map(|i| submit(&format!("call-{i}"), serde_json::json!({})))
            .collect(),
    ));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        0,
    );

    for _ in 0..3 {
        assert!(matches!(
            convo
                .next_attempt()
                .await
                .expect("invalid attempt returned"),
            AnswerAttempt::InvalidSubmission
        ));
    }
    assert_eq!(
        provider.calls(),
        3,
        "one provider request per answer attempt"
    );
}

/// The final permitted exploration turn does not consume the answer request that follows
/// it. Exploration is hidden from the caller, and only `submit` remains on that request.
#[tokio::test]
async fn the_last_tool_turn_can_be_followed_by_a_submission() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        MockResponse::ToolCalls(vec![(
            "call-tool".into(),
            "lookup".into(),
            serde_json::json!({}),
        )]),
        submit(
            "call-2",
            serde_json::json!({ "classes": ["schema:Person"] }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());
    let mut tools = AgentToolRegistry::empty();
    tools
        .register_required(Arc::new(MockInternalTool::new(
            "lookup",
            vec!["found".into()],
        )))
        .unwrap();

    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        tools,
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        1,
    );

    assert!(matches!(
        convo
            .next_attempt()
            .await
            .expect("submission after exploration"),
        AnswerAttempt::Submitted(_)
    ));
    assert_eq!(provider.calls(), 2);
    let tool_histories = provider.tool_histories();
    assert!(tool_histories[0].iter().any(|tool| tool.name == "lookup"));
    assert_eq!(
        tool_histories[1]
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        [SUBMIT_TOOL_NAME],
        "the request after the final tool turn offers only submit"
    );
}

/// Domain feedback answers only the valid submission that the caller rejected.
#[tokio::test]
async fn a_rejected_submission_is_answered_before_the_next_attempt() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(vec![
        submit("call-1", serde_json::json!({ "classes": ["schema:Thing"] })),
        submit(
            "call-2",
            serde_json::json!({ "classes": ["schema:Person"] }),
        ),
    ]));
    let registry = test_registry_with_group("mock", provider.clone(), "test", test_model_group());
    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        0,
    );

    assert!(matches!(
        convo.next_attempt().await.expect("first submission"),
        AnswerAttempt::Submitted(_)
    ));
    convo
        .reject_submission("schema:Thing is too broad")
        .unwrap();
    assert!(matches!(
        convo.next_attempt().await.expect("revised submission"),
        AnswerAttempt::Submitted(_)
    ));
    assert!(answered(&provider.last_history(), "call-1"));
}

#[tokio::test]
async fn rejection_without_a_pending_submission_is_refused() {
    let db = test_db().await;
    let usage = test_usage_service(&db);
    let provider = Arc::new(MockModelProvider::new(Vec::new()));
    let registry = test_registry_with_group("mock", provider, "test", test_model_group());
    let mut convo = StructuredConversation::<Classification>::new(
        &registry,
        &usage,
        AgentToolRegistry::empty(),
        mock_context(),
        test_model_group(),
        "system".into(),
        "classify this".into(),
        test_usage_ctx(),
        0,
    );

    let err = convo
        .reject_submission("nothing to reject")
        .expect_err("no pending submit");
    assert!(err.to_string().contains("no submitted answer"));
}
