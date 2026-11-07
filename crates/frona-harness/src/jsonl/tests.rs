use super::*;
use crate::{
    ActionStatus, DefaultBudget, EffectState, ExecutionControl, ExecutionStop, HistoryOperation,
    ModelOutput, PersistenceOutcome, ToolDispatchError, ToolRegistry, UsageReport, tool,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
struct FaultFile {
    file: File,
    fail: HistoryWritePhase,
    record: usize,
    fail_record: usize,
    operations: Arc<Mutex<Vec<HistoryWritePhase>>>,
}
impl Write for FaultFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.record += 1;
        self.operations
            .lock()
            .unwrap()
            .push(HistoryWritePhase::Append);
        if self.record == self.fail_record && self.fail == HistoryWritePhase::Append {
            self.file.write_all(&bytes[..bytes.len() / 2])?;
            return Err(io::Error::other("injected partial append"));
        }
        self.file.write_all(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.operations
            .lock()
            .unwrap()
            .push(HistoryWritePhase::Flush);
        if self.record == self.fail_record && self.fail == HistoryWritePhase::Flush {
            return Err(io::Error::other("injected flush"));
        }
        self.file.flush()
    }
}
impl DurableWrite for FaultFile {
    fn synchronize(&self) -> io::Result<()> {
        self.operations
            .lock()
            .unwrap()
            .push(HistoryWritePhase::Synchronize);
        if self.record == self.fail_record && self.fail == HistoryWritePhase::Synchronize {
            return Err(io::Error::other("injected sync"));
        }
        self.file.sync_all()
    }
}

#[tokio::test]
async fn real_writer_failures_stop_before_effect_or_preserve_returned_effect() {
    for phase in [
        HistoryWritePhase::Append,
        HistoryWritePhase::Flush,
        HistoryWritePhase::Synchronize,
    ] {
        for fail_record in [3, 4] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("history.jsonl");
            let operations = Arc::new(Mutex::new(Vec::new()));
            let writer = JsonlWriter {
                file: Box::new(FaultFile {
                    file: File::create(&path).unwrap(),
                    fail: phase,
                    record: 0,
                    fail_record,
                    operations: operations.clone(),
                }),
            };
            let mut conversation = Conversation::new("input");
            conversation.attach_jsonl(writer).unwrap();
            let effects = AtomicUsize::new(0);
            let mut tools = ToolRegistry::default();
            tools
                .register(
                    tool("effect", "Count effects", |_: serde_json::Value| async {
                        effects.fetch_add(1, Ordering::SeqCst);
                        Ok("returned output")
                    })
                    .unwrap(),
                )
                .unwrap();
            let actions = conversation
                .record_response(
                    ModelOutput::new(
                        vec![AssistantContent::ToolCall(ToolCall::from_wire(
                            "one",
                            ToolFunction::new("effect".into(), json!({})),
                        ))],
                        UsageReport::Unavailable,
                    ),
                    tools.definitions().cloned().collect(),
                )
                .unwrap();
            let action = actions.actions.into_iter().next().unwrap();
            let budget = DefaultBudget::default();
            let error = conversation
                .execute_tool(&action, &mut tools, ExecutionControl::new(&budget))
                .await
                .unwrap_err();
            let ToolDispatchError::Stop(ExecutionStop::Storage(failure)) = error else {
                panic!("expected typed storage stop")
            };
            let failure = failure.downcast_ref::<crate::HistoryFailure>().unwrap();
            assert_eq!(failure.phase, phase);
            assert_eq!(failure.persistence, PersistenceOutcome::Unknown);
            assert_eq!(failure.attempted_seq, fail_record as u64 - 1);
            assert_eq!(failure.acknowledged_seq, Some(fail_record as u64 - 2));
            assert_eq!(
                failure.operation,
                if fail_record == 3 {
                    HistoryOperation::ToolStart
                } else {
                    HistoryOperation::ActionResolution
                }
            );
            assert_eq!(
                failure.effect,
                if fail_record == 3 {
                    EffectState::Prevented
                } else {
                    EffectState::Returned
                }
            );
            assert_eq!(
                effects.load(Ordering::SeqCst),
                usize::from(fail_record == 4)
            );
            assert!(conversation.pending_write().is_some());
            let operation_count = operations.lock().unwrap().len();
            assert!(conversation.stop().is_err());
            assert_eq!(
                operations.lock().unwrap().len(),
                operation_count,
                "uncertain writer must be disabled"
            );
            let loaded = Jsonl::open(&path).unwrap();
            assert_eq!(
                effects.load(Ordering::SeqCst),
                usize::from(fail_record == 4),
                "load cannot retry effects"
            );
            let status = loaded.conversation().responses()[0].actions()[0].status();
            assert_eq!(
                status,
                match (fail_record, phase) {
                    (3, HistoryWritePhase::Append) => ActionStatus::Proposed,
                    (4, HistoryWritePhase::Append) | (3, _) => ActionStatus::Started,
                    (4, _) => ActionStatus::Resolved,
                    _ => unreachable!(),
                }
            );
            assert_eq!(
                loaded.incomplete_tail().is_some(),
                phase == HistoryWritePhase::Append
            );
            let saved_bytes = std::fs::read(&path).unwrap();
            let mut memory = conversation.into_memory().unwrap();
            if fail_record == 4 {
                memory.adopt_pending_write().unwrap();
                assert_eq!(memory.pending_count(), 0);
                assert_eq!(effects.load(Ordering::SeqCst), 1);
                memory.finish().unwrap();
            } else {
                memory.discard_pending_write().unwrap();
                assert_eq!(memory.pending_count(), 1);
            }
            assert_eq!(
                std::fs::read(&path).unwrap(),
                saved_bytes,
                "explicit memory continuation leaves uncertain file untouched"
            );
            let operations = operations.lock().unwrap();
            for complete in operations[..(fail_record - 1) * 3].chunks(3) {
                assert_eq!(
                    complete,
                    [
                        HistoryWritePhase::Append,
                        HistoryWritePhase::Flush,
                        HistoryWritePhase::Synchronize
                    ]
                );
            }
        }
    }
}

#[test]
fn attaching_does_not_freeze_mode_or_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let mut conversation = Conversation::new("question");
    conversation
        .attach_jsonl(Jsonl::create(&path).unwrap())
        .unwrap();
    let submission = crate::submission_tool::<String>();
    conversation.configure_history("standing instructions", Some(&submission));
    conversation.ensure_history().unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    let crate::HistoryEvent::Opened {
        instructions, mode, ..
    } = &loaded.conversation().history()[0].event
    else {
        panic!("expected opened")
    };
    assert_eq!(instructions, "standing instructions");
    assert_eq!(mode, &crate::HistoryMode::Structured(submission.parameters));
}
