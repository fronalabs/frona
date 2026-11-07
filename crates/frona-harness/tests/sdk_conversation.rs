use frona_harness::{
    ActionKind, ActionResolution, ActionStatus, Conversation, ConversationError,
    ConversationStatus, ModelOutput, ToolContent, ToolOutput, UsageReport,
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{
        DocumentSourceKind, Image, ImageMediaType, Reasoning, ToolCall, ToolFunction, UserContent,
    },
    request::ToolDefinition,
};
use serde_json::{Value, json};

fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.into(), arguments),
    ))
}
fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(content, UsageReport::Unavailable)
}
fn result(text: &str) -> ActionResolution {
    ActionResolution::Result(ToolOutput::text(text))
}
fn offered(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: "application schema".into(),
        parameters: json!({"type":"object"}),
    }
}

#[test]
fn records_every_mixed_proposal_without_decoding_dispatch_or_mutating_arguments() {
    let mut conversation = Conversation::new("investigate");
    let mut response = output(vec![
        AssistantContent::text("exploring"),
        call(
            "lookup-1",
            "lookup",
            json!({"description":"legitimate argument", "key":1}),
        ),
        call("submit-1", "submit", json!({"answer":1})),
        call("submit-2", "submit", json!("malformed object")),
        call("unknown-1", "unknown", json!([1, 2])),
    ]);
    response.metadata = json!({"provider_field":{"untouched":true}});
    let original = response.content.clone();
    let pending = conversation
        .record_response(response, vec![offered("lookup"), offered("submit")])
        .unwrap();
    assert_eq!(pending.actions.len(), 4);
    assert_eq!(conversation.pending_count(), 4);
    let expected = [
        ("lookup", ActionKind::Tool),
        ("submit", ActionKind::Submission),
        ("submit", ActionKind::Submission),
        ("unknown", ActionKind::Tool),
    ];
    for (handle, (name, kind)) in pending.actions.iter().zip(expected) {
        let view = conversation.action(handle).unwrap();
        assert_eq!(view.call.function.name, name);
        assert_eq!(view.kind, kind);
        assert_eq!(view.status, ActionStatus::Proposed);
        assert_eq!(view.offered_tools.len(), 2);
    }
    assert_eq!(
        conversation
            .action(&pending.actions[0])
            .unwrap()
            .call
            .function
            .arguments["description"],
        "legitimate argument"
    );
    assert_eq!(conversation.responses()[0].output().content, original);
    assert_eq!(
        conversation.responses()[0].output().metadata,
        json!({"provider_field":{"untouched":true}})
    );
    assert_eq!(conversation.messages().len(), 2);
    assert!(
        matches!(&conversation.messages()[1], Message::Assistant {content,..} if content == &original)
    );
    assert_eq!(
        conversation.request_messages().unwrap_err(),
        ConversationError::PendingActions(4)
    );
    assert_eq!(
        conversation.finish().unwrap_err(),
        ConversationError::PendingActions(4)
    );
}

#[test]
fn resolves_out_of_order_into_one_original_order_result_batch_with_rich_content() {
    let image = Image {
        data: DocumentSourceKind::Base64("aW1hZ2U=".into()),
        media_type: Some(ImageMediaType::PNG),
        ..Default::default()
    };
    let signed = ToolCall::from_dual_wire(
        "item-1",
        "call-1",
        ToolFunction::new("lookup".into(), json!({"description":"keep"})),
    )
    .with_signature(Some("opaque+/=\nunchanged".into()))
    .with_additional_params(Some(json!({"opaque":"call metadata"})));
    let contents = vec![
        AssistantContent::Reasoning(Reasoning::new_with_signature(
            "thinking",
            Some("reason-signature".into()),
        )),
        AssistantContent::Image(image.clone()),
        AssistantContent::ToolCall(signed.clone()),
        call("call-2", "submit", json!({"answer":42})),
    ];
    let mut conversation = Conversation::new("hello");
    let pending = conversation
        .record_response(output(contents.clone()), vec![])
        .unwrap();
    conversation
        .resolve(
            &pending.actions[1],
            ActionResolution::Rejected(ToolOutput::text("revise")),
        )
        .unwrap();
    assert_eq!(
        conversation.messages().len(),
        2,
        "partial result batches are withheld"
    );
    assert_eq!(
        conversation.responses()[0].actions()[1].status(),
        ActionStatus::Rejected
    );
    let rich = ToolOutput::content(vec![
        ToolContent::Image(image),
        ToolContent::Json {
            value: json!({"found":true}),
        },
    ])
    .with_metadata(json!({"private":"result metadata"}));
    conversation.start(&pending.actions[0]).unwrap();
    conversation
        .resolve(&pending.actions[0], ActionResolution::Result(rich.clone()))
        .unwrap();
    assert_eq!(conversation.messages().len(), 3);
    assert!(
        matches!(&conversation.messages()[1], Message::Assistant {content,..} if content == &contents)
    );
    let Message::User { content } = &conversation.messages()[2] else {
        panic!("result batch missing")
    };
    assert_eq!(content.len(), 2);
    let UserContent::ToolResult(first) = &content[0] else {
        panic!("tool result missing")
    };
    assert_eq!(first.call, signed.id);
    assert_eq!(first.provider, signed.provider);
    assert_eq!(first.name, "lookup");
    assert_eq!(first.content, rich.content);
    assert!(
        matches!(&content[1], UserContent::ToolResult(result) if result.call.as_str() == "call-2")
    );
    assert_eq!(
        conversation.responses()[0].actions()[0]
            .resolution()
            .unwrap()
            .output()
            .metadata,
        rich.metadata
    );
    let history = conversation.request_messages().unwrap();
    let encoded = serde_json::to_value(history).unwrap();
    let decoded: Vec<Message> = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, history);
}

#[test]
fn duplicate_and_absent_provider_ids_do_not_conflate_action_slots() {
    let mut conversation = Conversation::default();
    let pending = conversation
        .record_response(
            output(vec![
                call("same", "lookup", json!({"key":1})),
                call("same", "lookup", json!({"key":2})),
                call("", "submit", json!({"answer":3})),
            ]),
            vec![],
        )
        .unwrap();
    assert_eq!(pending.actions.len(), 3);
    assert!(
        conversation
            .action(&pending.actions[2])
            .unwrap()
            .call
            .provider
            .is_none()
    );
    for (index, handle) in pending.actions.iter().enumerate() {
        conversation
            .resolve(handle, result(&index.to_string()))
            .unwrap();
    }
    let Message::User { content } = &conversation.messages()[1] else {
        panic!()
    };
    assert_eq!(content.len(), 3);
    let UserContent::ToolResult(first) = &content[0] else {
        panic!()
    };
    let UserContent::ToolResult(second) = &content[1] else {
        panic!()
    };
    assert_eq!(
        first.call, second.call,
        "raw IDs are not repaired or rewritten"
    );
    assert_ne!(first.content, second.content);
}

#[test]
fn foreign_stale_and_resolved_handles_cannot_change_state() {
    let mut a = Conversation::default();
    let pending = a
        .record_response(output(vec![call("a", "lookup", json!({}))]), vec![])
        .unwrap();
    let handle = &pending.actions[0];
    let mut b = Conversation::default();
    b.record_response(output(vec![call("a", "lookup", json!({}))]), vec![])
        .unwrap();
    assert_eq!(
        b.action(handle).unwrap_err(),
        ConversationError::ForeignAction
    );
    assert_eq!(
        b.start(handle).unwrap_err(),
        ConversationError::ForeignAction
    );
    assert_eq!(
        b.resolve(handle, result("wrong")).unwrap_err(),
        ConversationError::ForeignAction
    );
    assert_eq!(b.pending_count(), 1);
    a.start(handle).unwrap();
    assert_eq!(
        a.start(handle).unwrap_err(),
        ConversationError::AlreadyStarted
    );
    assert_eq!(
        a.resolve(
            handle,
            ActionResolution::Rejected(ToolOutput::text("too late"))
        )
        .unwrap_err(),
        ConversationError::AlreadyStarted
    );
    a.resolve(handle, result("done")).unwrap();
    assert_eq!(
        a.resolve(handle, result("twice")).unwrap_err(),
        ConversationError::AlreadyResolved
    );
    assert_eq!(a.messages().len(), 2);
    a.record_response(output(vec![call("a", "lookup", json!({}))]), vec![])
        .unwrap();
    assert_eq!(a.start(handle).unwrap_err(), ConversationError::StaleAction);
    assert_eq!(
        a.resolve(handle, result("stale")).unwrap_err(),
        ConversationError::StaleAction
    );
    assert_eq!(a.pending_count(), 1);
}

#[test]
fn terminal_states_invalidate_handles_without_resolving_or_accepting_work() {
    let mut stopped = Conversation::default();
    let pending = stopped
        .record_response(
            output(vec![call("a", "submit", json!({"candidate":1}))]),
            vec![],
        )
        .unwrap();
    stopped.stop();
    assert_eq!(stopped.status(), ConversationStatus::Stopped);
    assert_eq!(stopped.pending_count(), 1);
    assert_eq!(
        stopped.action(&pending.actions[0]).unwrap_err(),
        ConversationError::Terminal
    );
    assert_eq!(
        stopped.start(&pending.actions[0]).unwrap_err(),
        ConversationError::Terminal
    );
    assert_eq!(
        stopped
            .resolve(&pending.actions[0], result("accepted?"))
            .unwrap_err(),
        ConversationError::Terminal
    );
    assert_eq!(
        stopped.responses()[0].actions()[0].status(),
        ActionStatus::Proposed
    );
    assert_eq!(
        stopped.record_response(output(vec![]), vec![]).unwrap_err(),
        ConversationError::Terminal
    );
    let mut finished = Conversation::default();
    let pending = finished
        .record_response(output(vec![call("b", "lookup", json!({}))]), vec![])
        .unwrap();
    finished
        .resolve(&pending.actions[0], result("done"))
        .unwrap();
    finished.finish().unwrap();
    assert_eq!(finished.status(), ConversationStatus::Finished);
    assert_eq!(
        finished
            .resolve(&pending.actions[0], result("again"))
            .unwrap_err(),
        ConversationError::Terminal
    );
    finished.stop();
    assert_eq!(finished.status(), ConversationStatus::Finished);
}

#[test]
fn native_structured_and_empty_content_remain_raw_without_invented_actions() {
    let mut response = output(vec![]);
    response.structured = Some(json!({"answer":42}));
    response.metadata = json!({"id":"provider-response"});
    let mut conversation = Conversation::default();
    let pending = conversation.record_response(response, vec![]).unwrap();
    assert!(pending.actions.is_empty());
    assert_eq!(
        conversation.responses()[0].output().structured,
        Some(json!({"answer":42}))
    );
    assert_eq!(
        conversation.responses()[0].output().metadata,
        json!({"id":"provider-response"})
    );
    assert!(
        matches!(&conversation.messages()[0], Message::Assistant { content, .. } if content.is_empty())
    );
    assert!(conversation.request_messages().is_ok());
}

#[test]
fn new_messages_and_responses_cannot_bypass_pending_pairing() {
    let mut conversation = Conversation::default();
    let pending = conversation
        .record_response(output(vec![call("a", "lookup", json!({}))]), vec![])
        .unwrap();
    assert_eq!(
        conversation
            .push_user(vec![UserContent::text("later")])
            .unwrap_err(),
        ConversationError::PendingActions(1)
    );
    assert_eq!(
        conversation
            .record_response(output(vec![]), vec![])
            .unwrap_err(),
        ConversationError::PendingActions(1)
    );
    conversation
        .resolve(&pending.actions[0], result("done"))
        .unwrap();
    assert_eq!(
        conversation
            .push_user(vec![UserContent::tool_result(
                "invented",
                "lookup",
                vec![ToolContent::text("invented")]
            )])
            .unwrap_err(),
        ConversationError::UnpairedToolResult
    );
    conversation
        .push_user(vec![UserContent::text("revise without invented call")])
        .unwrap();
    assert_eq!(conversation.messages().len(), 3);
    assert!(
        matches!(&conversation.messages()[2], Message::User {content} if matches!(&content[0], UserContent::Text(_)))
    );
}
