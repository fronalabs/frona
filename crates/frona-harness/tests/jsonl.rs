#![cfg(feature = "jsonl")]
use frona_harness::{
    ActionResolution, ActionStatus, CompactionLimits, Conversation, ConversationStatus,
    HistoryCheckpoint, HistoryDecodeErrorKind, HistoryEvent, HistoryRecord, Jsonl, JsonlError,
    JsonlOperation, ModelOutput, ToolOutput, UsageReport, encode_jsonl,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction, UserContent},
};
use serde_json::json;
use std::{fs, io::Write};

fn response(content: AssistantContent) -> ModelOutput {
    ModelOutput::new(vec![content], UsageReport::Unavailable)
}
fn append(records: &mut Vec<HistoryRecord>, event: HistoryEvent) {
    records.push(HistoryRecord {
        version: 2,
        conversation: records[0].conversation,
        seq: records.len() as u64,
        event,
    });
}

#[test]
fn create_attach_append_reopen_and_continue_same_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let writer = Jsonl::create(&path).unwrap();
    assert!(matches!(
        Jsonl::create(&path),
        Err(JsonlError::Io {
            operation: JsonlOperation::Create,
            ..
        })
    ));
    assert!(
        matches!(Jsonl::open(&path), Err(JsonlError::Decode { source, .. }) if source.kind == HistoryDecodeErrorKind::MissingOpened)
    );
    let mut conversation = Conversation::new("initial");
    conversation.attach_jsonl(writer).unwrap();
    let actions = conversation
        .record_response(
            response(AssistantContent::ToolCall(ToolCall::from_wire(
                "id",
                ToolFunction::new("tool".try_into().unwrap(), json!({"raw":true})),
            ))),
            vec![],
        )
        .unwrap();
    let action = actions.actions.into_iter().next().unwrap();
    conversation.start(&action).unwrap();
    let expected = encode_jsonl(conversation.history()).unwrap();
    drop(conversation);
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), expected);
    assert_eq!(
        loaded.conversation().responses()[0].actions()[0].status(),
        ActionStatus::Started
    );
    assert!(loaded.incomplete_tail().is_none());
    let mut conversation = loaded.into_writable().unwrap();
    let action = conversation
        .pending_actions()
        .unwrap()
        .actions
        .into_iter()
        .next()
        .unwrap();
    conversation
        .resolve(
            &action,
            ActionResolution::Result(ToolOutput::text("saved result")),
        )
        .unwrap();
    conversation.finish().unwrap();
    let expected = encode_jsonl(conversation.history()).unwrap();
    drop(conversation);
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(loaded.conversation().status(), ConversationStatus::Finished);
    assert_eq!(
        encode_jsonl(loaded.conversation().history()).unwrap(),
        expected
    );
    let terminal = loaded.into_writable().unwrap();
    assert_eq!(terminal.status(), ConversationStatus::Finished);
}

#[test]
fn fresh_writer_cannot_replace_writer_or_existing_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut fresh = Conversation::new("input");
    fresh
        .attach_jsonl(Jsonl::create(dir.path().join("one")).unwrap())
        .unwrap();
    assert!(matches!(
        fresh.attach_jsonl(Jsonl::create(dir.path().join("two")).unwrap()),
        Err(JsonlError::AlreadyAttached)
    ));
    fresh.push_user(vec![UserContent::text("later")]).unwrap();
    assert!(matches!(
        fresh.attach_jsonl(Jsonl::create(dir.path().join("three")).unwrap()),
        Err(JsonlError::AlreadyAttached)
    ));
    let mut memory = Conversation::new("input");
    memory.push_user(vec![UserContent::text("later")]).unwrap();
    assert!(matches!(
        memory.attach_jsonl(Jsonl::create(dir.path().join("four")).unwrap()),
        Err(JsonlError::AlreadyAttached)
    ));
}

#[test]
fn open_preserves_tail_and_only_explicit_continuation_truncates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let mut original = Conversation::new("input");
    original
        .attach_jsonl(Jsonl::create(&path).unwrap())
        .unwrap();
    original
        .push_user(vec![UserContent::text("feedback")])
        .unwrap();
    let prefix = fs::read(&path).unwrap();
    drop(original);
    let tail = br#"{"valid":"json without LF"}"#;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail)
        .unwrap();
    let before = fs::read(&path).unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(loaded.incomplete_tail(), Some(prefix.len()..before.len()));
    assert_eq!(fs::read(&path).unwrap(), before, "open must not repair");
    let mut continued = loaded.into_writable().unwrap();
    assert_eq!(fs::read(&path).unwrap(), prefix);
    continued
        .push_user(vec![UserContent::text("continued")])
        .unwrap();
    assert_eq!(
        Jsonl::open(&path).unwrap().conversation().messages(),
        continued.messages()
    );
}

#[test]
fn changed_file_is_never_truncated_or_appended() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    fs::write(
        &path,
        include_bytes!("fixtures/history/completed-checkpoint-v1.jsonl"),
    )
    .unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    let changed = fs::read(&path).unwrap();
    assert!(matches!(
        loaded.into_writable(),
        Err(JsonlError::Changed { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), changed);
}

#[test]
fn corruption_version_and_io_errors_remain_structured() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        matches!(Jsonl::open(dir.path().join("missing")), Err(JsonlError::Io { operation: JsonlOperation::Open, source, .. }) if source.kind() == std::io::ErrorKind::NotFound)
    );
    assert!(matches!(
        Jsonl::create(dir.path().join("missing/child")),
        Err(JsonlError::Io {
            operation: JsonlOperation::Create,
            ..
        })
    ));
    let path = dir.path().join("history.jsonl");
    for (bytes, expected) in [
        (b"bad\n".as_slice(), HistoryDecodeErrorKind::Corruption),
        (
            b"{\"version\":9}\n".as_slice(),
            HistoryDecodeErrorKind::UnsupportedVersion,
        ),
    ] {
        fs::write(&path, bytes).unwrap();
        assert!(
            matches!(Jsonl::open(&path), Err(JsonlError::Decode { source, .. }) if source.kind == expected)
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn compaction_and_checkpoint_evidence_survive_file_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let mut original = Conversation::new("old input ".repeat(100));
    original
        .record_response(
            response(AssistantContent::text("old answer ".repeat(100))),
            vec![],
        )
        .unwrap();
    original
        .push_user(vec![UserContent::text("active")])
        .unwrap();
    let view = original
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
        .finish("facts", None)
        .unwrap();
    let mut records = original.history().to_vec();
    append(
        &mut records,
        HistoryEvent::Compacted {
            view: view.clone(),
            summarizer: "summary".into(),
            output: response(AssistantContent::text("facts")),
        },
    );
    let through_seq = records.last().unwrap().seq;
    append(
        &mut records,
        HistoryEvent::Checkpoint {
            checkpoint: HistoryCheckpoint {
                schema: "loop".into(),
                version: 2,
                payload: json!({"candidate":7}),
                through_seq,
            },
        },
    );
    fs::write(&path, encode_jsonl(&records).unwrap()).unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(loaded.conversation().messages(), original.messages());
    assert_eq!(loaded.conversation().compacted_view(), Some(&view));
    assert_eq!(
        loaded.checkpoint().unwrap().1.payload,
        json!({"candidate":7})
    );
    assert!(!loaded.checkpoint_stale());
    let mut writable = loaded.into_writable().unwrap();
    writable
        .push_user(vec![UserContent::text("later")])
        .unwrap();
    drop(writable);
    let loaded = Jsonl::open(&path).unwrap();
    assert!(loaded.checkpoint_stale());
    assert_eq!(loaded.conversation().compacted_view(), Some(&view));
    assert_eq!(loaded.checkpoint().unwrap().1.through_seq, through_seq);
    let before = fs::read(&path).unwrap();
    let mut memory = loaded.into_memory().conversation;
    memory
        .push_user(vec![UserContent::text("memory only")])
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn a_relative_filename_has_a_durable_parent_directory() {
    // TempPath removes the unique test file without changing the process cwd.
    let path = tempfile::NamedTempFile::new_in(".")
        .unwrap()
        .into_temp_path();
    fs::remove_file(&path).unwrap();
    let relative = std::path::Path::new(path.file_name().unwrap());
    let mut conversation = Conversation::new("relative");
    conversation
        .attach_jsonl(Jsonl::create(relative).unwrap())
        .unwrap();
    conversation.finish().unwrap();
    assert_eq!(
        Jsonl::open(relative).unwrap().conversation().status(),
        ConversationStatus::Finished
    );
}
