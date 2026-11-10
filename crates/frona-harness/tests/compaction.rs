#[path = "helpers/native.rs"]
mod native;

use frona_harness::{
    ActionResolution, CompactionError, CompactionLimits, Conversation, HistoryView,
    HistoryViewEntry, ModelOutput, ToolContent, ToolOutput, UsageReport,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{
        DocumentSourceKind, Image, ImageMediaType, Reasoning, ToolCall, ToolFunction, UserContent,
    },
    request::ToolDefinition,
};
use serde_json::json;

fn limits() -> CompactionLimits {
    CompactionLimits {
        available_input_tokens: 400,
        target_input_tokens: 60,
    }
}
fn old() -> Vec<Message> {
    vec![
        Message::user("older input ".repeat(100)),
        Message::assistant("older response ".repeat(100)),
    ]
}
fn call(item: &str, id: &str, name: &str) -> ToolCall {
    native::call(
        ToolCall::from_wire(
            id,
            ToolFunction::new(name.try_into().unwrap(), json!({"query":"unchanged"})),
        ),
        json!({"id":item, "call_id":id, "signature":"opaque+/=\nsignature", "opaque":"metadata"}),
    )
}
fn image() -> Image {
    Image {
        data: DocumentSourceKind::Base64("aW1hZ2U=".into()),
        media_type: Some(ImageMediaType::PNG),
        ..Default::default()
    }
}
fn exchange(contents: Vec<AssistantContent>) -> Vec<Message> {
    let mut conversation = Conversation::new("current request");
    let actions = conversation
        .record_response(ModelOutput::new(contents, UsageReport::Unavailable), vec![])
        .unwrap();
    for action in &actions.actions {
        conversation
            .resolve(
                action,
                ActionResolution::Result(ToolOutput::content(vec![
                    ToolContent::Image(image()),
                    ToolContent::Text("result".into()),
                ])),
            )
            .unwrap();
    }
    conversation.messages().to_vec()
}

#[test]
fn active_attachments_and_signed_tool_groups_are_retained_exactly() {
    let mut originals = old();
    let mut active = exchange(vec![
        AssistantContent::Reasoning(Reasoning::new("reasoning")).with_native(
            json!({"type":"thinking", "thinking":"reasoning", "signature":"reason-signature"}),
        ),
        AssistantContent::ToolCall(call("item", "call", "lookup")),
    ]);
    active[0] = Message::User {
        content: vec![UserContent::Image(image())],
    };
    originals.extend(active.clone());
    let before = serde_json::to_value(&originals).unwrap();
    let view = HistoryView::new(&originals);
    let plan = view
        .plan("standing prompt", &[], limits())
        .unwrap()
        .unwrap();
    assert_eq!(plan.covered_original_positions(), [0, 1]);
    assert_eq!(plan.summary_input(), &originals[..3]);
    assert_eq!(plan.summary_input_original_positions(), [0, 1, 2]);
    let compacted = plan
        .finish("older facts", Some("<context>frozen</context>"))
        .unwrap();
    let view = HistoryView::from_compacted(&originals, &compacted).unwrap();
    let messages = view.messages();
    assert_eq!(&messages[1..], active);
    assert_eq!(
        messages[0],
        Message::user(
            "<conversation_summary>\nolder facts\n\n<context>frozen</context>\n</conversation_summary>"
        )
    );
    assert_eq!(view.original_positions(), [2, 3, 4]);
    assert_eq!(serde_json::to_value(view.originals()).unwrap(), before);
}

#[test]
fn text_with_results_protects_the_entire_preceding_call_group() {
    let mut originals = old();
    let mut group = exchange(vec![AssistantContent::ToolCall(call(
        "item", "id", "lookup",
    ))]);
    if let Message::User { content } = group.last_mut().unwrap() {
        content.push(UserContent::text("latest instruction alongside results"));
    }
    originals.extend(group.clone());
    let plan = HistoryView::new(&originals)
        .plan("", &[], limits())
        .unwrap()
        .unwrap();
    assert!(!plan.covered_original_positions().contains(&3));
    let compacted = plan.finish("summary", None).unwrap();
    let materialized = HistoryView::from_compacted(&originals, &compacted)
        .unwrap()
        .messages();
    assert!(materialized.ends_with(&group[1..]));
}

#[test]
fn duplicate_ids_remain_distinct_and_results_match_call_id_and_name() {
    let mut originals = old();
    originals.extend(exchange(vec![
        AssistantContent::ToolCall(call("item-a", "same", "first")),
        AssistantContent::ToolCall(call("item-b", "same", "second")),
        AssistantContent::ToolCall(call("item-b", "same", "second")),
    ]));
    let plan = HistoryView::new(&originals)
        .plan("", &[], limits())
        .unwrap()
        .unwrap();
    let compacted = plan.finish("summary", None).unwrap();
    assert_eq!(
        &HistoryView::from_compacted(&originals, &compacted)
            .unwrap()
            .messages()[1..],
        &originals[2..]
    );
    let mut incorrect = originals.clone();
    if let Message::User { content } = incorrect.last_mut().unwrap()
        && let UserContent::ToolResult(result) = &mut content[0]
    {
        result.call = rig_core::completion::message::CallId::from_wire("wrong-call");
    }
    assert!(matches!(
        HistoryView::new(&incorrect).plan("", &[], limits()),
        Err(CompactionError::InvalidHistory(_))
    ));
    let mut incorrect = originals.clone();
    if let Message::User { content } = incorrect.last_mut().unwrap()
        && let UserContent::ToolResult(result) = &mut content[0]
    {
        result.name = "wrong-tool".try_into().unwrap();
    }
    assert!(matches!(
        HistoryView::new(&incorrect).plan("", &[], limits()),
        Err(CompactionError::InvalidHistory(_))
    ));
}

#[test]
fn incomplete_or_unpaired_protocol_cannot_be_hidden_in_summary() {
    let mut originals = old();
    let group = exchange(vec![AssistantContent::ToolCall(call(
        "item", "id", "lookup",
    ))]);
    originals.extend_from_slice(&group[..2]);
    let before = originals.clone();
    assert!(matches!(
        HistoryView::new(&originals).plan("", &[], limits()),
        Err(CompactionError::InvalidHistory(_))
    ));
    assert_eq!(originals, before);
    let mut unpaired = old();
    unpaired.push(group[2].clone());
    unpaired.push(Message::user("current"));
    assert!(matches!(
        HistoryView::new(&unpaired).plan("", &[], limits()),
        Err(CompactionError::InvalidHistory(_))
    ));
}

#[test]
fn repeated_compaction_keeps_transitive_coverage_and_only_literal_positions() {
    let mut originals = old();
    originals.push(Message::user("first active request"));
    let first = HistoryView::new(&originals)
        .plan("", &[], limits())
        .unwrap()
        .unwrap()
        .finish("first summary", None)
        .unwrap();
    originals.push(Message::assistant("another long answer ".repeat(100)));
    originals.push(Message::user("second active request"));
    let view = HistoryView::from_compacted(&originals, &first).unwrap();
    let plan = view.plan("", &[], limits()).unwrap().unwrap();
    assert_eq!(plan.covered_original_positions(), [0, 1, 2, 3]);
    assert_eq!(
        plan.summary_input()[0],
        Message::user("<conversation_summary>\nfirst summary\n</conversation_summary>")
    );
    let second = plan
        .finish("second summary", Some("new frozen context"))
        .unwrap();
    let view = HistoryView::from_compacted(&originals, &second).unwrap();
    assert_eq!(view.original_positions(), [4]);
    assert!(
        matches!(&view.entries()[0], HistoryViewEntry::Summary { covered_original_positions, .. } if covered_original_positions == &[0,1,2,3])
    );
    assert!(HistoryView::from_compacted(&originals[..2], &second).is_err());
    // A completed response appends after the fixed prefix; it cannot move it.
    originals.push(Message::assistant("completed answer"));
    let after = HistoryView::from_compacted(&originals, &second)
        .unwrap()
        .messages();
    assert_eq!(
        after[0],
        Message::user(
            "<conversation_summary>\nsecond summary\n\nnew frozen context\n</conversation_summary>"
        )
    );
    assert_eq!(after.last(), originals.last());
}

#[test]
fn oversized_or_empty_summary_never_changes_the_original_view() {
    let mut originals = old();
    originals.push(Message::user("current"));
    let view = HistoryView::new(&originals);
    let plan = view.plan("", &[], limits()).unwrap().unwrap();
    assert_eq!(plan.finish(" ", None), Err(CompactionError::EmptySummary));
    assert!(matches!(
        plan.finish(&"huge".repeat(1000), None),
        Err(CompactionError::ContextOverflow { .. })
    ));
    assert!(matches!(
        plan.finish("small", Some(&"context".repeat(1000))),
        Err(CompactionError::ContextOverflow { .. })
    ));
    assert_eq!(view.messages(), originals);
    // The final input allowance is inclusive.
    let small = plan.finish("summary", None).unwrap();
    let exact = HistoryView::from_compacted(&originals, &small)
        .unwrap()
        .estimated_tokens("", &[]);
    let exact_plan = view
        .plan(
            "",
            &[],
            CompactionLimits {
                available_input_tokens: exact,
                target_input_tokens: exact,
            },
        )
        .unwrap()
        .unwrap();
    assert!(exact_plan.finish("summary", None).is_ok());
}

#[test]
fn current_request_and_request_overhead_cannot_be_trimmed_to_fit() {
    let originals = vec![Message::user("current ".repeat(100))];
    assert!(matches!(
        HistoryView::new(&originals).plan(
            "",
            &[],
            CompactionLimits {
                available_input_tokens: 10,
                target_input_tokens: 5
            }
        ),
        Err(CompactionError::ContextOverflow { .. })
    ));
    let tools = vec![ToolDefinition {
        annotations: Default::default(),
        name: "lookup".try_into().unwrap(),
        description: "description ".repeat(100),
        parameters: json!({"type":"object"}),
    }];
    let originals = vec![Message::user("current")];
    assert!(matches!(
        HistoryView::new(&originals).plan(
            "",
            &tools,
            CompactionLimits {
                available_input_tokens: 20,
                target_input_tokens: 10
            }
        ),
        Err(CompactionError::ContextOverflow { .. })
    ));
    assert!(
        HistoryView::new(&originals)
            .plan("", &[], limits())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        HistoryView::new(&originals).plan(
            "",
            &[],
            CompactionLimits {
                available_input_tokens: 10,
                target_input_tokens: 11
            }
        ),
        Err(CompactionError::InvalidLimits)
    ));
}

#[test]
fn a_single_input_refinement_loop_compacts_completed_revisions_and_preserves_input() {
    let mut conversation = Conversation::new("Produce a correct answer from the attached evidence");
    for round in 0..5 {
        let actions = conversation
            .record_response(
                ModelOutput::new(
                    vec![AssistantContent::ToolCall(ToolCall::from_wire(
                        format!("submit-{round}"),
                        ToolFunction::new(
                            "submit".try_into().unwrap(),
                            json!({"draft":"candidate ".repeat(30)}),
                        ),
                    ))],
                    UsageReport::Unavailable,
                ),
                vec![],
            )
            .unwrap();
        conversation
            .revise_submission(&actions.actions[0], "Revision feedback ".repeat(20))
            .unwrap();
    }
    let mut originals = conversation.messages().to_vec();
    if let Message::User { content } = &mut originals[0] {
        content.push(UserContent::Image(image()));
    }
    let before = originals.clone();
    let view = HistoryView::new(&originals);
    assert!(view.estimated_tokens("", &[]) > 400);
    let plan = view.plan("", &[], limits()).unwrap().unwrap();
    assert!(!plan.covered_original_positions().contains(&0));
    assert_eq!(plan.summary_input_original_positions().first(), Some(&0));
    let compacted = plan
        .finish("Earlier revisions rejected incomplete evidence", None)
        .unwrap();
    let view = HistoryView::from_compacted(&originals, &compacted).unwrap();
    assert_eq!(view.messages()[1], originals[0]);
    assert_eq!(view.original_positions(), [0, 9, 10]);
    assert_eq!(view.messages()[2..], originals[9..]);
    assert_eq!(originals, before);
    assert!(view.estimated_tokens("", &[]) < 400);

    // A second round of compaction flattens old summary provenance but does not
    // claim the summarized feedback was included verbatim in the summary call.
    let actions = conversation
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    "next",
                    ToolFunction::new("submit".try_into().unwrap(), json!({"draft":"next"})),
                ))],
                UsageReport::Unavailable,
            ),
            vec![],
        )
        .unwrap();
    conversation
        .revise_submission(&actions.actions[0], "new feedback ".repeat(30))
        .unwrap();
    originals.extend_from_slice(&conversation.messages()[11..]);
    let view = HistoryView::from_compacted(&originals, &compacted).unwrap();
    let next = view.plan("", &[], limits()).unwrap().unwrap();
    assert!(next.covered_original_positions().contains(&1));
    assert!(!next.summary_input_original_positions().contains(&1));
    assert!(next.summary_input_original_positions().contains(&0));
    let compacted = next.finish("Updated revision summary", None).unwrap();
    assert_eq!(
        HistoryView::from_compacted(&originals, &compacted)
            .unwrap()
            .original_positions(),
        [0, 11, 12]
    );
}

#[test]
fn restored_view_can_display_new_pending_actions_but_cannot_plan_over_them() {
    let mut originals = old();
    originals.push(Message::user("current"));
    let compacted = HistoryView::new(&originals)
        .plan("", &[], limits())
        .unwrap()
        .unwrap()
        .finish("facts", None)
        .unwrap();
    originals.push(Message::Assistant(
        rig_core::completion::message::AssistantMessage::new(vec![AssistantContent::ToolCall(
            call("item", "pending", "lookup"),
        )]),
    ));
    let view = HistoryView::from_compacted(&originals, &compacted).unwrap();
    assert_eq!(view.messages().last(), originals.last());
    assert!(matches!(
        view.plan("", &[], limits()),
        Err(CompactionError::InvalidHistory(_))
    ));
}

#[test]
fn legacy_truncation_still_rejects_duplicate_ids_while_sdk_keeps_distinct_actions() {
    let history = exchange(vec![
        AssistantContent::ToolCall(call("item-a", "same", "lookup")),
        AssistantContent::ToolCall(call("item-b", "same", "lookup")),
    ]);
    let error =
        frona_harness::inference::context::truncate_history(history.clone(), "", 10_000, 100, 100)
            .unwrap_err();
    assert!(error.contains("duplicate call ID"));
    assert!(
        HistoryView::new(&history)
            .plan("", &[], limits())
            .unwrap()
            .is_none()
    );
}
