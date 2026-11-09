use frona_harness::{inference::Usage, *};
use rig_core::completion::{
    AssistantContent, Message,
    message::{
        DocumentSourceKind, Image, ImageMediaType, Reasoning, ToolCall, ToolFunction, UserContent,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput {
        content,
        structured: Some(json!({"native": [true, null]})),
        usage: UsageReport::Reported(Usage {
            input_tokens: 17,
            output_tokens: 9,
            total_tokens: 26,
            ..Usage::new()
        }),
        metadata: json!({"provider": {"opaque": [null, "signature+/=\n"]}}),
        ttft_ms: Some(23),
    }
}
fn call(name: &str, args: Value) -> AssistantContent {
    AssistantContent::ToolCall(
        ToolCall::from_dual_wire(
            "item",
            "same-provider-id",
            ToolFunction::new(name.into(), args),
        )
        .with_signature(Some("opaque+/=\nsignature".into()))
        .with_additional_params(Some(json!({"provider-extension": [1, null]}))),
    )
}
fn opened(initial: Option<Vec<UserContent>>) -> HistoryRecord {
    HistoryRecord {
        version: 1,
        conversation: Uuid::nil(),
        seq: 0,
        event: HistoryEvent::Opened {
            initial,
            instructions: "keep instructions".into(),
            mode: HistoryMode::Text,
        },
    }
}
fn append(records: &mut Vec<HistoryRecord>, event: HistoryEvent) {
    records.push(HistoryRecord {
        version: 1,
        conversation: records[0].conversation,
        seq: records.len() as u64,
        event,
    });
}
fn fixture() -> Vec<HistoryRecord> {
    let mut records = vec![opened(Some(vec![UserContent::text("input")]))];
    append(
        &mut records,
        HistoryEvent::Response {
            response: 0,
            message: 1,
            origin: None,
            offered_tools: vec![submission_tool::<Answer>()],
            output: output(vec![
                call(
                    "lookup",
                    json!({"description":"raw", "nested": [null, false]}),
                ),
                call("submit", json!({"value": 7})),
            ]),
        },
    );
    records
}
fn position(slot: usize) -> ActionPosition {
    ActionPosition {
        conversation: Uuid::nil(),
        response: 0,
        action: slot,
    }
}
fn checkpoint(records: &mut Vec<HistoryRecord>, payload: Value) {
    let through_seq = records.len() as u64 - 1;
    append(
        records,
        HistoryEvent::Checkpoint {
            checkpoint: HistoryCheckpoint {
                schema: "application/vocabulary".into(),
                version: 8,
                payload,
                through_seq,
            },
        },
    );
}
fn assert_state_error(records: &[HistoryRecord]) {
    let error = decode_jsonl(&encode_jsonl(records).unwrap()).unwrap_err();
    assert_eq!(error.kind, HistoryDecodeErrorKind::InvalidState, "{error}");
    assert!(error.byte.is_some());
}
#[derive(Debug, Deserialize, JsonSchema)]
struct Answer {
    value: u32,
}

#[test]
fn rich_source_history_roundtrips_exactly_and_batches_in_proposal_order() {
    let image = Image {
        data: DocumentSourceKind::Base64("aW1hZ2U=".into()),
        media_type: Some(ImageMediaType::PNG),
        ..Default::default()
    };
    let mut source = Conversation::new("input");
    let actions = source
        .record_response(
            output(vec![
                AssistantContent::Reasoning(Reasoning::new_with_signature(
                    "thinking",
                    Some("reason-signature".into()),
                )),
                AssistantContent::Image(image.clone()),
                call("lookup", json!("raw string, not coerced")),
                call("lookup", json!([null, 2])),
            ]),
            vec![submission_tool::<Answer>()],
        )
        .unwrap()
        .actions;
    source
        .resolve(
            &actions[1],
            ActionResolution::Rejected(ToolOutput::text("policy feedback")),
        )
        .unwrap();
    source.start(&actions[0]).unwrap();
    source
        .resolve(
            &actions[0],
            ActionResolution::Failed(
                ToolOutput::content(vec![
                    ToolContent::Image(image),
                    ToolContent::Json {
                        value: json!({"partial":true}),
                    },
                ])
                .with_metadata(json!({"error":"remote failed"})),
            ),
        )
        .unwrap();
    let encoded = encode_jsonl(source.history()).unwrap();
    for _ in 0..2 {
        let decoded = decode_jsonl(&encoded).unwrap();
        let restored = decoded.replay.conversation;
        assert_eq!(restored.messages(), source.messages());
        assert_eq!(encode_jsonl(restored.history()).unwrap(), encoded);
        assert_eq!(
            serde_json::to_value(restored.responses()[0].output()).unwrap(),
            serde_json::to_value(source.responses()[0].output()).unwrap()
        );
        assert_eq!(restored.responses()[0].actions()[0].invocation_ordinal(), 1);
        assert_eq!(
            restored.responses()[0].actions()[0].resolution(),
            source.responses()[0].actions()[0].resolution()
        );
        let Message::User { content } = &restored.messages()[2] else {
            panic!()
        };
        assert_eq!(content.len(), 2);
        assert_eq!(restored.pending_count(), 0);
        assert!(decoded.incomplete_tail.is_none());
    }
}

#[test]
fn default_and_empty_initial_content_keep_distinct_original_positions() {
    let mut empty = Conversation::default();
    empty.push_user(vec![]).unwrap();
    let decoded = decode_jsonl(&encode_jsonl(empty.history()).unwrap()).unwrap();
    assert_eq!(decoded.replay.conversation.messages(), empty.messages());
    assert!(matches!(
        decoded.replay.conversation.history()[0].event,
        HistoryEvent::Opened { initial: None, .. }
    ));
    for initial in [None, Some(vec![])] {
        let expected = usize::from(initial.is_some());
        let replay = replay_history(&[opened(initial)]).unwrap();
        assert_eq!(replay.conversation.messages().len(), expected);
    }
}

#[test]
fn proposed_started_restarted_and_partial_resolution_remain_distinct() {
    let mut records = fixture();
    let proposed = replay_history(&records).unwrap().conversation;
    assert_eq!(proposed.pending_count(), 2);
    assert!(
        proposed.responses()[0]
            .actions()
            .iter()
            .all(|a| a.status() == ActionStatus::Proposed)
    );
    append(
        &mut records,
        HistoryEvent::ToolStarted {
            action: position(0),
            ordinal: 1,
        },
    );
    append(
        &mut records,
        HistoryEvent::Stopped {
            category: FailureKind::Cancelled,
            message: "interrupted".into(),
        },
    );
    let stopped = replay_history(&records).unwrap().conversation;
    assert_eq!(stopped.status(), ConversationStatus::Stopped);
    assert_eq!(
        stopped.responses()[0].actions()[0].status(),
        ActionStatus::Started
    );
    append(
        &mut records,
        HistoryEvent::ToolStarted {
            action: position(0),
            ordinal: 2,
        },
    );
    append(
        &mut records,
        HistoryEvent::ActionResolved {
            action: position(1),
            resolution: ActionResolution::Rejected(ToolOutput::text("revise")),
        },
    );
    let partial = replay_history(&records).unwrap().conversation;
    assert_eq!(partial.messages().len(), 2);
    assert_eq!(partial.pending_count(), 1);
    assert_eq!(partial.responses()[0].actions()[0].invocation_ordinal(), 2);
    assert_eq!(partial.pending_actions().unwrap().actions.len(), 1);
    append(
        &mut records,
        HistoryEvent::ActionResolved {
            action: position(0),
            resolution: ActionResolution::Result(ToolOutput::text("done")),
        },
    );
    assert_eq!(
        replay_history(&records)
            .unwrap()
            .conversation
            .messages()
            .len(),
        3
    );
}

#[test]
fn pending_validation_is_never_restored_and_low_level_completions_are_honest() {
    let mut source = Conversation::new("low-level text mode");
    let action = source
        .record_response(output(vec![call("submit", json!({"value":7}))]), vec![])
        .unwrap()
        .actions
        .remove(0);
    assert!(matches!(
        source.submission::<Answer>(&action).unwrap(),
        Submission::Ready(Answer { value: 7 })
    ));
    assert!(source.action(&action).unwrap().submission_validated);
    let mut decoded = decode_jsonl(&encode_jsonl(source.history()).unwrap())
        .unwrap()
        .replay
        .conversation;
    let restored = decoded.pending_actions().unwrap().actions.remove(0);
    assert!(!decoded.action(&restored).unwrap().submission_validated);
    assert!(matches!(
        decoded.accept_submission(&restored, "must not accept"),
        Err(SubmissionError::NotValidated)
    ));
    source.accept_submission(&action, "caller owns A").unwrap();
    for kind in [
        SubmissionCompletion::Application,
        SubmissionCompletion::Schema,
    ] {
        let mut records = source.history().to_vec();
        let HistoryEvent::SubmissionCompleted { kind: saved, .. } =
            &mut records.last_mut().unwrap().event
        else {
            panic!()
        };
        *saved = kind;
        let finished = decode_jsonl(&encode_jsonl(&records).unwrap())
            .unwrap()
            .replay
            .conversation;
        assert_eq!(finished.status(), ConversationStatus::Finished);
        assert!(!finished.responses()[0].actions()[0].submission_validated());
        assert_eq!(finished.messages(), source.messages());
        assert!(
            matches!(finished.history().last().unwrap().event, HistoryEvent::SubmissionCompleted { kind: k, .. } if k == kind)
        );
    }
}

#[test]
fn checkpoints_have_exact_prefix_association_and_never_promote_candidates() {
    let mut records = fixture();
    checkpoint(&mut records, json!({"stage":"candidate", "value":7}));
    let saved = replay_history(&records).unwrap();
    assert_eq!(saved.checkpoint.as_ref().unwrap().0, 2);
    assert_eq!(saved.checkpoint.as_ref().unwrap().1.through_seq, 1);
    assert!(!saved.checkpoint_stale);
    assert_eq!(saved.conversation.status(), ConversationStatus::Active);
    assert_eq!(saved.conversation.pending_count(), 2);
    append(
        &mut records,
        HistoryEvent::ActionResolved {
            action: position(0),
            resolution: ActionResolution::Result(ToolOutput::text("result")),
        },
    );
    assert!(replay_history(&records).unwrap().checkpoint_stale);
    append(
        &mut records,
        HistoryEvent::SubmissionCompleted {
            action: position(1),
            acknowledgement: ToolOutput::text("exact saved ack")
                .with_metadata(json!({"receipt":3})),
            kind: SubmissionCompletion::Application,
        },
    );
    checkpoint(&mut records, json!({"stage":"complete"}));
    let finished = replay_history(&records).unwrap();
    assert!(!finished.checkpoint_stale);
    assert_eq!(finished.conversation.status(), ConversationStatus::Finished);
    assert_eq!(
        finished.checkpoint.unwrap().1.payload,
        json!({"stage":"complete"})
    );
    let mut invalid = records.clone();
    let HistoryEvent::Checkpoint { checkpoint } = &mut invalid.last_mut().unwrap().event else {
        panic!()
    };
    checkpoint.through_seq -= 1;
    assert_state_error(&invalid);
    for event in [
        HistoryEvent::Stopped {
            category: FailureKind::ApplicationStop,
            message: "late".into(),
        },
        HistoryEvent::Finished {
            kind: HistoryCompletion::Protocol,
        },
        HistoryEvent::User {
            message: 3,
            content: vec![],
        },
    ] {
        let mut invalid = records.clone();
        append(&mut invalid, event);
        assert_state_error(&invalid);
    }
}

#[test]
fn incomplete_tail_and_committed_corruption_are_different_outcomes() {
    let records = fixture();
    let prefix = encode_jsonl(&records[..1]).unwrap();
    let full = encode_jsonl(&records).unwrap();
    for suffix in [
        b"{\"version\":1".as_slice(),
        &full[prefix.len()..full.len() - 1],
        &[0xff, 0xfe],
    ] {
        let mut bytes = prefix.clone();
        bytes.extend(suffix);
        let decoded = decode_jsonl(&bytes).unwrap();
        assert_eq!(decoded.incomplete_tail, Some(prefix.len()..bytes.len()));
        assert_eq!(decoded.replay.conversation.history().len(), 1);
    }
    for suffix in [b"{bad}\n".as_slice(), b"\n", &[0xff, b'\n']] {
        let mut bytes = prefix.clone();
        bytes.extend(suffix);
        bytes.extend(&full[prefix.len()..]);
        let error = decode_jsonl(&bytes).unwrap_err();
        assert_eq!(error.kind, HistoryDecodeErrorKind::Corruption);
        assert_eq!(error.byte, Some(prefix.len()));
        assert_eq!(error.seq, 1);
    }
    for bytes in [b"".as_slice(), &prefix[..prefix.len() - 1]] {
        assert_eq!(
            decode_jsonl(bytes).unwrap_err().kind,
            HistoryDecodeErrorKind::MissingOpened
        );
    }
    assert_eq!(
        decode_jsonl(&full)
            .unwrap()
            .replay
            .conversation
            .history()
            .len(),
        2
    );
}

#[test]
fn format_versions_positions_and_envelopes_fail_without_guessing() {
    let valid = fixture();
    let line = serde_json::to_value(&valid[0]).unwrap();
    for (field, value, expected) in [
        (
            "version",
            json!(2),
            HistoryDecodeErrorKind::UnsupportedVersion,
        ),
        (
            "version",
            json!(u64::MAX),
            HistoryDecodeErrorKind::UnsupportedVersion,
        ),
        ("version", json!("1"), HistoryDecodeErrorKind::Format),
        (
            "event",
            json!({"Unknown":{}}),
            HistoryDecodeErrorKind::Format,
        ),
        (
            "event",
            json!({"Opened":{"instructions":"", "mode":"Text"}}),
            HistoryDecodeErrorKind::Format,
        ),
        ("seq", json!(-1), HistoryDecodeErrorKind::Format),
    ] {
        let mut changed = line.clone();
        changed[field] = value;
        let mut bytes = serde_json::to_vec(&changed).unwrap();
        bytes.push(b'\n');
        assert_eq!(decode_jsonl(&bytes).unwrap_err().kind, expected);
    }
    for seq in [0, 2, u64::MAX] {
        let mut records = valid.clone();
        records[1].seq = seq;
        assert_state_error(&records);
    }
    let mut wrong_id = valid.clone();
    wrong_id[1].conversation = Uuid::from_u128(1);
    assert_state_error(&wrong_id);
    let mut duplicate = valid.clone();
    append(&mut duplicate, valid[0].event.clone());
    assert_state_error(&duplicate);
    for field in ["message", "response"] {
        let mut value = serde_json::to_value(&valid[1]).unwrap();
        value["event"]["Response"][field] = json!(u64::MAX);
        let mut bytes = encode_jsonl(&valid[..1]).unwrap();
        bytes.extend(serde_json::to_vec(&value).unwrap());
        bytes.push(b'\n');
        let error = decode_jsonl(&bytes).unwrap_err();
        assert!(matches!(
            error.kind,
            HistoryDecodeErrorKind::InvalidState | HistoryDecodeErrorKind::Format
        ));
    }
    let mut value = serde_json::to_value(&valid[1]).unwrap();
    value["event"]["Response"]
        .as_object_mut()
        .unwrap()
        .remove("origin");
    let mut bytes = encode_jsonl(&valid[..1]).unwrap();
    bytes.extend(serde_json::to_vec(&value).unwrap());
    bytes.push(b'\n');
    assert_eq!(
        decode_jsonl(&bytes).unwrap_err().kind,
        HistoryDecodeErrorKind::Format
    );
}

#[test]
fn lifecycle_errors_never_turn_incomplete_work_into_completion() {
    for event in [
        HistoryEvent::ToolStarted {
            action: position(1),
            ordinal: 1,
        },
        HistoryEvent::ToolStarted {
            action: position(0),
            ordinal: 0,
        },
        HistoryEvent::ToolStarted {
            action: position(0),
            ordinal: 2,
        },
        HistoryEvent::ActionResolved {
            action: position(2),
            resolution: ActionResolution::Result(ToolOutput::text("unknown")),
        },
        HistoryEvent::SubmissionCompleted {
            action: position(0),
            acknowledgement: ToolOutput::text("wrong kind"),
            kind: SubmissionCompletion::Schema,
        },
        HistoryEvent::SubmissionCompleted {
            action: position(1),
            acknowledgement: ToolOutput::text("siblings pending"),
            kind: SubmissionCompletion::Application,
        },
        HistoryEvent::Finished {
            kind: HistoryCompletion::Protocol,
        },
        HistoryEvent::User {
            message: 2,
            content: vec![],
        },
        HistoryEvent::Response {
            response: 1,
            message: 2,
            origin: None,
            offered_tools: vec![],
            output: output(vec![]),
        },
    ] {
        let mut records = fixture();
        append(&mut records, event);
        assert_state_error(&records);
    }
    let mut started = fixture();
    append(
        &mut started,
        HistoryEvent::ToolStarted {
            action: position(0),
            ordinal: 1,
        },
    );
    for event in [
        HistoryEvent::ToolStarted {
            action: position(0),
            ordinal: 1,
        },
        HistoryEvent::ActionResolved {
            action: position(0),
            resolution: ActionResolution::Rejected(ToolOutput::text("cannot reject started")),
        },
    ] {
        let mut invalid = started.clone();
        append(&mut invalid, event);
        assert_state_error(&invalid);
    }
    let mut resolved = fixture();
    let resolution = HistoryEvent::ActionResolved {
        action: position(0),
        resolution: ActionResolution::Result(ToolOutput::text("caller result")),
    };
    append(&mut resolved, resolution.clone());
    append(&mut resolved, resolution);
    assert_state_error(&resolved);
}

#[test]
fn text_completion_uses_the_driver_final_response_rule() {
    let mut records = vec![opened(None)];
    append(
        &mut records,
        HistoryEvent::Finished {
            kind: HistoryCompletion::Text { response: 0 },
        },
    );
    assert_state_error(&records);
    records.pop();
    append(
        &mut records,
        HistoryEvent::Response {
            response: 0,
            message: 0,
            origin: None,
            offered_tools: vec![],
            output: output(vec![AssistantContent::text("")]),
        },
    );
    append(
        &mut records,
        HistoryEvent::Finished {
            kind: HistoryCompletion::Text { response: 0 },
        },
    );
    assert_state_error(&records);
    let HistoryEvent::Response { output, .. } = &mut records[1].event else {
        panic!()
    };
    output.content = vec![AssistantContent::text("answer")];
    assert_eq!(
        replay_history(&records).unwrap().conversation.status(),
        ConversationStatus::Finished
    );
    records.last_mut().unwrap().event = HistoryEvent::Finished {
        kind: HistoryCompletion::Text { response: 1 },
    };
    assert_state_error(&records);
    records.last_mut().unwrap().event = HistoryEvent::Finished {
        kind: HistoryCompletion::Protocol,
    };
    assert_eq!(
        replay_history(&records).unwrap().conversation.status(),
        ConversationStatus::Finished
    );
}

#[test]
fn compaction_replays_the_exact_view_and_preserves_all_originals_and_metadata() {
    let mut conversation = Conversation::new("old request ".repeat(100));
    conversation
        .record_response(
            output(vec![AssistantContent::text("old answer ".repeat(100))]),
            vec![],
        )
        .unwrap();
    conversation
        .push_user(vec![UserContent::text("active request")])
        .unwrap();
    let view = conversation
        .model_view()
        .unwrap()
        .plan(
            "",
            &[],
            CompactionLimits {
                available_input_tokens: 200,
                target_input_tokens: 50,
            },
        )
        .unwrap()
        .unwrap()
        .finish("old facts", Some("frozen context"))
        .unwrap();
    let expected = HistoryView::from_compacted(conversation.messages(), &view)
        .unwrap()
        .messages();
    let mut records = conversation.history().to_vec();
    let summary = output(vec![AssistantContent::text("old facts")]);
    append(
        &mut records,
        HistoryEvent::Compacted {
            view: view.clone(),
            summarizer: "local/summary".into(),
            output: summary,
        },
    );
    let bytes = encode_jsonl(&records).unwrap();
    let replayed = decode_jsonl(&bytes).unwrap().replay.conversation;
    assert_eq!(replayed.messages(), conversation.messages());
    assert_eq!(replayed.request_messages().unwrap(), expected);
    assert_eq!(replayed.compacted_view(), Some(&view));
    assert_eq!(encode_jsonl(replayed.history()).unwrap(), bytes);
    let mut malformed = serde_json::to_value(records.last().unwrap()).unwrap();
    malformed["event"]["Compacted"]["view"]["source_message_count"] = json!(999);
    let mut bytes = encode_jsonl(&records[..records.len() - 1]).unwrap();
    bytes.extend(serde_json::to_vec(&malformed).unwrap());
    bytes.push(b'\n');
    assert_eq!(
        decode_jsonl(&bytes).unwrap_err().kind,
        HistoryDecodeErrorKind::InvalidState
    );
}

#[test]
fn golden_version_one_wire_and_duplicate_fields_are_not_normalized() {
    let golden = include_bytes!("fixtures/history/completed-checkpoint-v1.jsonl");
    let decoded = decode_jsonl(golden).unwrap();
    assert_eq!(
        decoded.replay.conversation.status(),
        ConversationStatus::Finished
    );
    assert_eq!(
        decoded.replay.checkpoint.unwrap().1.payload,
        json!({"done": true})
    );
    assert_eq!(
        encode_jsonl(decoded.replay.conversation.history()).unwrap(),
        golden
    );
    let original = String::from_utf8(encode_jsonl(&[opened(None)]).unwrap()).unwrap();
    for (from, to) in [
        ("\"seq\":0", "\"seq\":0,\"seq\":0"),
        ("\"version\":1", "\"version\":2,\"version\":1"),
        ("\"initial\":null", "\"initial\":null,\"initial\":[]"),
        ("\"mode\":\"Text\"", "\"mode\":\"Text\",\"mode\":\"Text\""),
    ] {
        let ambiguous = original.replace(from, to);
        assert_eq!(
            decode_jsonl(ambiguous.as_bytes()).unwrap_err().kind,
            HistoryDecodeErrorKind::Format
        );
    }
    let extra = original.replace(
        "\"mode\":\"Text\"",
        "\"mode\":\"Text\",\"submission_validated\":true",
    );
    assert!(
        decode_jsonl(extra.as_bytes()).is_ok(),
        "additional object fields are permitted without semantics"
    );
}

#[tokio::test]
async fn replay_never_calls_registered_models_tools_inspectors_or_restores_live_reports() {
    use async_trait::async_trait;
    use frona_harness::inference::{InferenceError, provider::StreamToken};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Adapter(Arc<AtomicUsize>);
    #[async_trait]
    impl ModelAdapter for Adapter {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tools: true,
                ..Capabilities::default()
            }
        }
        async fn generate(
            &self,
            _: ModelRequest,
            _: Option<tokio::sync::mpsc::Sender<StreamToken>>,
        ) -> Result<ModelOutput, InferenceError> {
            let turn = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(output(vec![if turn == 0 {
                call("lookup", json!({"value":7}))
            } else {
                call("submit", json!({"value":7}))
            }]))
        }
    }
    let models = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(AtomicUsize::new(0));
    let inspections = AtomicUsize::new(0);
    let model = Model::from_adapter("fake", "replay-effects", Adapter(models.clone())).unwrap();
    let harness = Harness::new(model);
    let invoked = tools.clone();
    let run = harness
        .run("work")
        .disable_compaction()
        .structured::<Answer>()
        .tool(
            tool("lookup", "side effect", move |args: Answer| {
                invoked.fetch_add(1, Ordering::SeqCst);
                async move { Ok(args.value) }
            })
            .unwrap(),
        );
    let report = run.report();
    let mut session = run.manual();
    let first = session.call_model().await.unwrap().actions.remove(0);
    session.execute(first).await.unwrap();
    let submission = session.call_model().await.unwrap().actions.remove(0);
    assert!(matches!(
        session.submission(&submission).unwrap(),
        Submission::Ready(_)
    ));
    inspections.fetch_add(1, Ordering::SeqCst);
    session.accept(submission, "application A").unwrap();
    let bytes = encode_jsonl(session.conversation().history()).unwrap();
    let attempts = report.snapshot().provider_attempts();
    for _ in 0..3 {
        let restored = decode_jsonl(&bytes).unwrap().replay.conversation;
        assert_eq!(restored.messages(), session.conversation().messages());
        assert!(restored.history_failure().is_none());
        assert!(restored.detached_failures().is_empty());
        assert_eq!(models.load(Ordering::SeqCst), 2);
        assert_eq!(tools.load(Ordering::SeqCst), 1);
        assert_eq!(inspections.load(Ordering::SeqCst), 1);
        assert_eq!(report.snapshot().provider_attempts(), attempts);
    }
}
