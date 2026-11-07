use frona_harness::{
    ActionResolution, ActionStatus, Conversation, ConversationError, ConversationStatus,
    ModelOutput, Submission, SubmissionError, ToolOutput, UsageReport, submission_tool,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction, UserContent},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize, JsonSchema, PartialEq)]
struct Answer {
    classes: Vec<String>,
    #[serde(default)]
    relations: Vec<String>,
}

fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.into(), arguments),
    ))
}
fn record(
    conversation: &mut Conversation,
    content: Vec<AssistantContent>,
) -> Vec<frona_harness::PendingAction> {
    conversation
        .record_response(
            ModelOutput::new(content, UsageReport::Unavailable),
            vec![submission_tool::<Answer>()],
        )
        .unwrap()
        .actions
}
fn results(conversation: &Conversation) -> Vec<(&str, String)> {
    conversation
        .messages()
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content
                .iter()
                .filter_map(|item| match item {
                    UserContent::ToolResult(result) => Some((
                        result.call.as_str(),
                        result
                            .content
                            .iter()
                            .filter_map(|item| item.as_text())
                            .collect::<Vec<_>>()
                            .join(" "),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => vec![],
        })
        .collect()
}

#[test]
fn malformed_submission_repairs_its_call_and_waits_for_mixed_siblings() {
    let mut conversation = Conversation::new("classify");
    let actions = record(
        &mut conversation,
        vec![
            call("lookup", "lookup", json!({"description":"keep"})),
            call("bad", "submit", json!({"relations":[]})),
            call("other", "submit", json!([1, 2])),
        ],
    );
    let Submission::Repaired { feedback } = conversation.submission::<Answer>(&actions[1]).unwrap()
    else {
        panic!("expected repair")
    };
    assert!(feedback.contains("classes"));
    assert!(feedback.contains("relations"));
    assert!(results(&conversation).is_empty());
    assert_eq!(conversation.pending_count(), 2);
    assert!(matches!(
        conversation.submission::<Answer>(&actions[2]).unwrap(),
        Submission::Repaired { .. }
    ));
    assert!(conversation.request_messages().is_err());
    conversation
        .resolve(
            &actions[0],
            ActionResolution::Result(ToolOutput::text("found")),
        )
        .unwrap();
    assert_eq!(
        results(&conversation)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        ["lookup", "bad", "other"]
    );
    assert!(results(&conversation)[2].1.contains("array"));
    assert!(conversation.request_messages().is_ok());
    assert_eq!(
        conversation.responses()[0].actions()[1]
            .call()
            .function
            .arguments,
        json!({"relations":[]})
    );
}

#[test]
fn existing_wrapper_and_json_collection_repairs_preserve_original_arguments() {
    for args in [
        json!({"arbitrary":{"classes":["Person"]}}),
        json!({"classes":"[\"Person\"]"}),
    ] {
        let mut conversation = Conversation::new("classify");
        let actions = record(&mut conversation, vec![call("s", "submit", args.clone())]);
        let Submission::Ready(value) = conversation.submission::<Answer>(&actions[0]).unwrap()
        else {
            panic!("repair should decode")
        };
        assert_eq!(value.classes, ["Person"]);
        assert_eq!(
            conversation.responses()[0].actions()[0]
                .call()
                .function
                .arguments,
            args
        );
        assert_eq!(
            conversation.pending_count(),
            1,
            "decoding is not acceptance"
        );
        assert!(results(&conversation).is_empty());
    }
}

#[test]
fn serde_success_does_not_bypass_schema_constraints() {
    #[derive(Debug, Deserialize, JsonSchema)]
    struct Score {
        #[schemars(range(min = 1, max = 5))]
        value: u32,
    }
    assert_eq!(
        serde_json::from_value::<Score>(json!({"value":9}))
            .unwrap()
            .value,
        9
    );
    let mut conversation = Conversation::new("score");
    let actions = record(
        &mut conversation,
        vec![call("bad", "submit", json!({"value":9}))],
    );
    assert!(matches!(
        conversation.submission::<Score>(&actions[0]).unwrap(),
        Submission::Repaired { .. }
    ));
    assert_eq!(
        conversation.responses()[0].actions()[0].status(),
        ActionStatus::Rejected
    );
    assert_eq!(results(&conversation)[0].0, "bad");
    assert!(matches!(
        conversation.accept_submission(&actions[0], 9),
        Err(SubmissionError::Conversation(
            ConversationError::AlreadyResolved
        ))
    ));
}

#[test]
fn acceptance_requires_exact_validated_action_and_all_siblings_resolved() {
    let mut conversation = Conversation::new("classify");
    let original = json!({"classes":["Person"]});
    let actions = record(
        &mut conversation,
        vec![
            call("first", "submit", original.clone()),
            call("second", "submit", json!({"classes":["Place"]})),
            call("lookup", "lookup", json!({})),
        ],
    );
    assert!(matches!(
        conversation.accept_submission(&actions[0], "normalized"),
        Err(SubmissionError::NotValidated)
    ));
    let Submission::Ready(value) = conversation.submission::<Answer>(&actions[0]).unwrap() else {
        panic!()
    };
    assert!(matches!(
        conversation.accept_submission(&actions[1], "wrong"),
        Err(SubmissionError::NotValidated)
    ));
    assert!(matches!(
        conversation.accept_submission(&actions[0], "normalized"),
        Err(SubmissionError::Conversation(
            ConversationError::PendingActions(2)
        ))
    ));
    assert_eq!(conversation.pending_count(), 3);
    conversation
        .revise_submission(&actions[1], "Use the first candidate")
        .unwrap();
    conversation
        .resolve(
            &actions[2],
            ActionResolution::Rejected(ToolOutput::text("not needed")),
        )
        .unwrap();
    let accepted: usize = conversation
        .accept_submission(&actions[0], value.classes.len())
        .unwrap();
    assert_eq!(accepted, 1);
    assert_eq!(conversation.status(), ConversationStatus::Finished);
    assert_eq!(
        conversation.responses()[0].actions()[0]
            .call()
            .function
            .arguments,
        original
    );
    assert_eq!(
        results(&conversation)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        ["first", "second", "lookup"]
    );
    assert!(matches!(
        conversation.revise_submission(&actions[0], "late"),
        Err(SubmissionError::Conversation(ConversationError::Terminal))
    ));
}

#[test]
fn revision_allows_another_request_but_old_or_foreign_handles_cannot_accept() {
    let mut conversation = Conversation::new("classify");
    let actions = record(
        &mut conversation,
        vec![call("one", "submit", json!({"classes":[]}))],
    );
    assert!(matches!(
        conversation.submission::<Answer>(&actions[0]).unwrap(),
        Submission::Ready(_)
    ));
    conversation
        .revise_submission(&actions[0], "include more evidence")
        .unwrap();
    assert!(results(&conversation)[0].1.contains("more evidence"));
    assert!(matches!(
        conversation.revise_submission(&actions[0], "again"),
        Err(SubmissionError::Conversation(
            ConversationError::AlreadyResolved
        ))
    ));
    let next = record(
        &mut conversation,
        vec![call("two", "submit", json!({"classes":[]}))],
    );
    assert!(matches!(
        conversation.accept_submission(&actions[0], ()),
        Err(SubmissionError::Conversation(
            ConversationError::StaleAction
        ))
    ));
    let mut other = Conversation::new("other");
    assert!(matches!(
        other.submission::<Answer>(&next[0]),
        Err(SubmissionError::Conversation(
            ConversationError::ForeignAction
        ))
    ));
    conversation.stop();
    assert!(matches!(
        conversation.submission::<Answer>(&next[0]),
        Err(SubmissionError::Conversation(ConversationError::Terminal))
    ));
}

#[test]
fn missing_submission_is_conversational_feedback_without_invented_ids() {
    let mut conversation = Conversation::new("classify");
    assert!(matches!(
        conversation.repair_missing_submission(),
        Err(SubmissionError::NoResponse)
    ));
    record(
        &mut conversation,
        vec![AssistantContent::text("I think it is a person")],
    );
    conversation.repair_missing_submission().unwrap();
    assert!(results(&conversation).is_empty());
    assert_eq!(conversation.responses()[0].actions().len(), 0);
    let Message::User { content } = conversation.messages().last().unwrap() else {
        panic!()
    };
    assert!(matches!(&content[0], UserContent::Text(text) if text.text.contains("submit")));
    let actions = record(&mut conversation, vec![call("lookup", "lookup", json!({}))]);
    assert!(matches!(
        conversation.repair_missing_submission(),
        Err(SubmissionError::Conversation(
            ConversationError::PendingActions(1)
        ))
    ));
    assert!(matches!(
        conversation.revise_submission(&actions[0], "wrong"),
        Err(SubmissionError::NotSubmission)
    ));
    conversation
        .resolve(
            &actions[0],
            ActionResolution::Result(ToolOutput::text("found")),
        )
        .unwrap();
    conversation.repair_missing_submission().unwrap();
    let actions = record(
        &mut conversation,
        vec![call("s", "submit", json!({"classes":[]}))],
    );
    conversation
        .revise_submission(&actions[0], "revise")
        .unwrap();
    assert!(matches!(
        conversation.repair_missing_submission(),
        Err(SubmissionError::SubmissionPresent)
    ));
}

#[test]
fn invalid_schema_is_an_operational_error_without_repair_or_resolution() {
    #[derive(Debug, Deserialize)]
    struct InvalidSchema;
    impl JsonSchema for InvalidSchema {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            "InvalidSchema".into()
        }
        fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
            schemars::Schema::try_from(json!({"type":42})).unwrap()
        }
    }
    let mut conversation = Conversation::new("classify");
    let actions = record(&mut conversation, vec![call("s", "submit", json!({}))]);
    assert!(matches!(
        conversation.submission::<InvalidSchema>(&actions[0]),
        Err(SubmissionError::Schema(_))
    ));
    assert_eq!(conversation.pending_count(), 1);
    assert!(
        !conversation
            .action(&actions[0])
            .unwrap()
            .submission_validated
    );
    assert!(results(&conversation).is_empty());
}
