use frona_harness::{
    CheckpointLoadError, Conversation, ConversationError, ConversationStatus, DefaultBudget,
    EffectState, ExecutionControl, HistoryEvent, HistoryOperation, HistoryWritePhase,
    PersistenceOutcome, RecoveryError, ToolDispatchError, ToolRegistry, decode_jsonl, encode_jsonl,
};
use rig_core::completion::message::UserContent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct State {
    stage: String,
    candidate: Option<u64>,
}

fn state(candidate: u64) -> State {
    State {
        stage: "inspect".into(),
        candidate: Some(candidate),
    }
}

#[test]
fn checkpoint_roundtrips_exact_prefix_and_latest_stream_without_execution() {
    let mut conversation = Conversation::new("input");
    assert!(
        conversation
            .load_checkpoint::<State>("app", 1)
            .unwrap()
            .is_none()
    );
    assert!(
        conversation.history().is_empty(),
        "reading cannot create Opened"
    );
    let seq = conversation.save_checkpoint("app", 1, &state(7)).unwrap();
    assert_eq!(seq, 1);
    let loaded = conversation
        .load_checkpoint::<State>("app", 1)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.seq, seq);
    assert_eq!(loaded.through_seq, seq - 1);
    assert_eq!(loaded.payload, state(7));
    assert_eq!(conversation.messages().len(), 1);
    assert!(conversation.responses().is_empty());
    assert_eq!(conversation.status(), ConversationStatus::Active);

    let seq = conversation.save_checkpoint("app", 1, &state(8)).unwrap();
    assert_eq!(
        seq, 2,
        "consecutive checkpoints associate with the prior record"
    );
    let encoded = encode_jsonl(conversation.history()).unwrap();
    let replay = decode_jsonl(&encoded).unwrap().replay;
    assert_eq!(replay.checkpoint.as_ref().unwrap().0, seq);
    assert!(!replay.checkpoint_stale);
    assert_eq!(
        replay
            .conversation
            .load_checkpoint::<State>("app", 1)
            .unwrap()
            .unwrap()
            .payload,
        state(8)
    );
    assert_eq!(replay.conversation.messages(), conversation.messages());
}

#[test]
fn incompatible_stale_and_unreadable_are_distinct_and_do_not_fall_back() {
    let mut conversation = Conversation::new("input");
    conversation.save_checkpoint("app", 1, &state(7)).unwrap();
    conversation
        .push_user(vec![UserContent::text("later feedback")])
        .unwrap();
    assert!(matches!(
        conversation.load_checkpoint::<State>("app", 1),
        Err(CheckpointLoadError::Stale {
            seq: 1,
            through_seq: 0
        })
    ));
    let raw = conversation.checkpoint().unwrap();
    assert!(raw.stale);
    assert_eq!(
        raw.checkpoint.payload,
        json!({"stage":"inspect", "candidate":7})
    );
    let replay = decode_jsonl(&encode_jsonl(conversation.history()).unwrap())
        .unwrap()
        .replay;
    assert!(replay.checkpoint_stale);

    let seq = conversation
        .save_checkpoint("other-app", 2, &state(8))
        .unwrap();
    assert!(matches!(
        conversation.load_checkpoint::<State>("app", 1),
        Err(CheckpointLoadError::Incompatible { seq: found, schema, version: 2 })
            if found == seq && schema == "other-app"
    ));
    // Correct identity but incompatible payload; never return the earlier State.
    let seq = conversation
        .save_checkpoint("app", 1, &json!({"stage":false}))
        .unwrap();
    assert!(matches!(
        conversation.load_checkpoint::<State>("app", 1),
        Err(CheckpointLoadError::Decode { seq: found, .. }) if found == seq
    ));
    assert_eq!(
        conversation.checkpoint().unwrap().checkpoint.payload,
        json!({"stage":false})
    );
    assert!(
        conversation.history_failure().is_none(),
        "loading is read-only"
    );
    assert_eq!(conversation.status(), ConversationStatus::Active);
}

struct CannotSerialize<'a>(&'a AtomicUsize);
impl Serialize for CannotSerialize<'_> {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(serde::ser::Error::custom(
            "application payload cannot serialize",
        ))
    }
}

#[tokio::test]
async fn serialization_failure_stops_without_fabricated_payload_and_preserves_completion() {
    for terminal in [false, true] {
        let mut conversation = Conversation::new("input");
        #[cfg(feature = "jsonl")]
        let dir = tempfile::tempdir().unwrap();
        #[cfg(feature = "jsonl")]
        let path = dir.path().join("checkpoint.jsonl");
        #[cfg(feature = "jsonl")]
        conversation
            .attach_jsonl(frona_harness::Jsonl::create(&path).unwrap())
            .unwrap();
        conversation.save_checkpoint("app", 1, &state(7)).unwrap();
        if terminal {
            conversation.finish().unwrap();
        }
        let before = encode_jsonl(conversation.history()).unwrap();
        let acknowledged = conversation.history().last().unwrap().seq;
        let serializations = AtomicUsize::new(0);
        let error = conversation
            .save_checkpoint("app", 1, &CannotSerialize(&serializations))
            .unwrap_err();
        let ConversationError::Storage(failure) = error else {
            panic!("typed storage failure")
        };
        assert_eq!(failure.phase, HistoryWritePhase::Serialize);
        assert_eq!(failure.operation, HistoryOperation::Checkpoint);
        assert_eq!(failure.persistence, PersistenceOutcome::NotWritten);
        assert_eq!(failure.effect, EffectState::NoEffect);
        assert_eq!(failure.acknowledged_seq, Some(acknowledged));
        assert_eq!(failure.attempted_seq, acknowledged + 1);
        assert!(conversation.pending_write().is_none());
        assert_eq!(encode_jsonl(conversation.history()).unwrap(), before);
        assert_eq!(
            conversation.status(),
            if terminal {
                ConversationStatus::Finished
            } else {
                ConversationStatus::Stopped
            }
        );
        assert!(
            conversation
                .save_checkpoint("app", 1, &CannotSerialize(&serializations))
                .is_err()
        );
        assert_eq!(
            serializations.load(Ordering::SeqCst),
            1,
            "failed state must reject before serialization"
        );
        #[cfg(feature = "jsonl")]
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut memory = conversation.into_memory().unwrap();
        if terminal {
            assert_eq!(memory.status(), ConversationStatus::Finished);
            assert!(matches!(
                memory
                    .recover_tools(
                        &mut ToolRegistry::default(),
                        ExecutionControl::new(&DefaultBudget::default())
                    )
                    .await,
                Err(ToolDispatchError::Recovery(RecoveryError::AlreadyCompleted))
            ));
        }
        memory.save_checkpoint("app", 1, &state(9)).unwrap();
        assert_eq!(
            memory
                .load_checkpoint::<State>("app", 1)
                .unwrap()
                .unwrap()
                .payload,
            state(9)
        );
        assert_eq!(
            memory.status(),
            if terminal {
                ConversationStatus::Finished
            } else {
                ConversationStatus::Active
            }
        );
        #[cfg(feature = "jsonl")]
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn saving_after_completion_keeps_terminal_evidence_and_does_not_accept_candidate() {
    let mut conversation = Conversation::new("input");
    conversation.finish().unwrap();
    let seq = conversation.save_checkpoint("app", 1, &state(42)).unwrap();
    assert!(matches!(
        conversation.history()[seq as usize - 1].event,
        HistoryEvent::Finished { .. }
    ));
    assert_eq!(conversation.status(), ConversationStatus::Finished);
    assert_eq!(
        conversation
            .load_checkpoint::<State>("app", 1)
            .unwrap()
            .unwrap()
            .payload,
        state(42)
    );
    let replay = decode_jsonl(&encode_jsonl(conversation.history()).unwrap())
        .unwrap()
        .replay;
    assert_eq!(replay.conversation.status(), ConversationStatus::Finished);
    assert!(!replay.checkpoint_stale);
}

#[test]
fn checkpoints_preserve_pending_submissions_and_incomplete_stops() {
    use frona_harness::{ActionStatus, ModelOutput, Submission, UsageReport, submission_tool};
    use rig_core::completion::{
        AssistantContent,
        message::{ToolCall, ToolFunction},
    };
    let mut conversation = Conversation::new("input");
    let actions = conversation
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    "submit-id",
                    ToolFunction::new("submit".into(), json!("candidate")),
                ))],
                UsageReport::Unavailable,
            ),
            vec![submission_tool::<String>()],
        )
        .unwrap();
    let submission = &actions.actions[0];
    assert!(matches!(
        conversation.submission::<String>(submission).unwrap(),
        Submission::Ready(_)
    ));
    conversation.save_checkpoint("app", 1, &state(7)).unwrap();
    assert_eq!(conversation.pending_count(), 1);
    assert_eq!(
        conversation.action(submission).unwrap().status,
        ActionStatus::Proposed
    );
    conversation.stop().unwrap();
    conversation.save_checkpoint("app", 1, &state(8)).unwrap();
    let replay = decode_jsonl(&encode_jsonl(conversation.history()).unwrap())
        .unwrap()
        .replay;
    assert_eq!(replay.conversation.status(), ConversationStatus::Stopped);
    assert_eq!(replay.conversation.pending_count(), 1);
    assert!(!replay.conversation.responses()[0].actions()[0].submission_validated());
    assert_eq!(
        replay
            .conversation
            .load_checkpoint::<State>("app", 1)
            .unwrap()
            .unwrap()
            .payload,
        state(8)
    );
    assert!(
        !replay
            .conversation
            .history()
            .iter()
            .any(|r| matches!(r.event, HistoryEvent::SubmissionCompleted { .. }))
    );
}

#[cfg(feature = "jsonl")]
#[test]
fn jsonl_typed_load_preserves_app_state_and_requires_explicit_writes() {
    use frona_harness::Jsonl;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.jsonl");
    let mut conversation = Conversation::new("input");
    conversation
        .attach_jsonl(Jsonl::create(&path).unwrap())
        .unwrap();
    conversation.save_checkpoint("app", 3, &state(7)).unwrap();
    drop(conversation);
    let before = std::fs::read(&path).unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(
        loaded
            .load_checkpoint::<State>("app", 3)
            .unwrap()
            .unwrap()
            .payload,
        state(7)
    );
    assert!(matches!(
        loaded.load_checkpoint::<State>("app", 2),
        Err(CheckpointLoadError::Incompatible { .. })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut writable = loaded.into_writable().unwrap();
    writable.save_checkpoint("app", 3, &state(8)).unwrap();
    assert_eq!(
        Jsonl::open(&path)
            .unwrap()
            .load_checkpoint::<State>("app", 3)
            .unwrap()
            .unwrap()
            .payload,
        state(8)
    );
}
