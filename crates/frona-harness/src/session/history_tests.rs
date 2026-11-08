use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::*;
use crate::{
    Capabilities, Decision, EffectState, HistoryEvent, HistoryOperation, HistoryRecord,
    HistoryWritePhase, ModelAdapter, ModelOutput, PersistenceOutcome, SubmissionCompletion,
    UsageReport,
    history::{HistoryWriteError, HistoryWriter},
    inference::{InferenceError, provider::StreamToken},
    tool,
};

#[derive(Debug)]
struct FaultStore {
    records: Arc<Mutex<Vec<HistoryRecord>>>,
    fail: HistoryOperation,
    persist_failed: bool,
    cancel: Option<CancellationToken>,
}
impl HistoryWriter for FaultStore {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        if record.event.operation() == self.fail {
            if self.persist_failed {
                self.records.lock().unwrap().push(record.clone());
            }
            if let Some(token) = &self.cancel {
                token.cancel();
            }
            return Err(HistoryWriteError {
                phase: HistoryWritePhase::Synchronize,
                cause: "injected sync failure".into(),
            });
        }
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}
fn fault<Mode>(
    mut run: Run<'static, Mode>,
    fail: HistoryOperation,
) -> (Run<'static, Mode>, Arc<Mutex<Vec<HistoryRecord>>>) {
    let records = Arc::new(Mutex::new(Vec::new()));
    run.core.conversation.attach_history(Box::new(FaultStore {
        records: records.clone(),
        fail,
        persist_failed: true,
        cancel: None,
    }));
    (run, records)
}
struct Fake {
    outputs: Mutex<VecDeque<ModelOutput>>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl ModelAdapter for Fake {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model effect"))
    }
}
fn model(outputs: Vec<ModelOutput>) -> (Model<'static>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        Model::from_adapter(
            "fake",
            "model",
            Fake {
                outputs: Mutex::new(outputs.into()),
                calls: calls.clone(),
            },
        )
        .unwrap(),
        calls,
    )
}
fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(content, UsageReport::Unavailable)
}
fn text(s: &str) -> ModelOutput {
    output(vec![AssistantContent::text(s)])
}
fn call(name: &str, args: Value) -> ModelOutput {
    output(vec![AssistantContent::ToolCall(ToolCall::from_wire(
        "id",
        ToolFunction::new(name.into(), args),
    ))])
}
fn storage(error: &ManualError) {
    assert!(
        matches!(error, ManualError::Stop(ExecutionStop::Storage(_))),
        "{error:?}"
    );
}
fn failed(conversation: &Conversation, op: HistoryOperation, effect: EffectState) {
    let failure = conversation.history_failure().unwrap();
    assert_eq!(failure.operation, op);
    assert_eq!(failure.effect, effect);
    assert_eq!(failure.persistence, PersistenceOutcome::Unknown);
    assert_eq!(
        conversation.pending_write().unwrap().seq,
        failure.attempted_seq
    );
    assert_eq!(
        conversation.history().last().map(|r| r.seq),
        failure.acknowledged_seq
    );
    assert_eq!(
        conversation.status(),
        super::super::ConversationStatus::Stopped
    );
}

#[tokio::test]
async fn open_and_response_failure_prevent_tools_and_further_models() {
    for operation in [HistoryOperation::Open, HistoryOperation::Response] {
        let (model, calls) = model(vec![call("effect", json!({}))]);
        let effects = Arc::new(AtomicUsize::new(0));
        let counted = effects.clone();
        let run = Harness::new(model).run("input").tool(
            tool("effect", "effect", move |_: Value| {
                counted.fetch_add(1, Ordering::SeqCst);
                async { Ok("done") }
            })
            .unwrap(),
        );
        let (run, saved) = fault(run, operation);
        let mut session = run.manual();
        storage(&session.call_model().await.unwrap_err());
        storage(&session.call_model().await.unwrap_err());
        failed(
            session.conversation(),
            operation,
            if operation == HistoryOperation::Open {
                EffectState::Prevented
            } else {
                EffectState::Returned
            },
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(operation == HistoryOperation::Response)
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(session.conversation().responses().len(), 0);
        assert!(
            saved.lock().unwrap().len() > session.conversation().history().len(),
            "a failed sync may still leave complete bytes"
        );
        assert!(matches!(
            session.report().snapshot().outcome,
            Some(RunOutcome::Incomplete {
                kind: super::super::FailureKind::Storage,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn start_and_result_failures_distinguish_prevented_and_returned_effects() {
    for operation in [
        HistoryOperation::ToolStart,
        HistoryOperation::ActionResolution,
    ] {
        let (model, calls) = model(vec![call("effect", json!({}))]);
        let effects = Arc::new(AtomicUsize::new(0));
        let counted = effects.clone();
        let run = Harness::new(model).run("input").tool(
            tool("effect", "effect", move |_: Value| {
                counted.fetch_add(1, Ordering::SeqCst);
                async { Ok("done") }
            })
            .unwrap(),
        );
        let (run, _) = fault(run, operation);
        let mut session = run.manual();
        let action = session.call_model().await.unwrap().actions.remove(0);
        storage(&session.execute(action).await.unwrap_err());
        storage(&session.call_model().await.unwrap_err());
        let returned = operation == HistoryOperation::ActionResolution;
        failed(
            session.conversation(),
            operation,
            if returned {
                EffectState::Returned
            } else {
                EffectState::Prevented
            },
        );
        assert_eq!(effects.load(Ordering::SeqCst), usize::from(returned));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.conversation().pending_count(), 1);
        assert!(
            session.conversation().responses()[0].actions()[0]
                .resolution()
                .is_none()
        );
    }
}

#[tokio::test]
async fn automatic_and_manual_have_identical_commits_and_failure_ordering() {
    for automatic in [false, true] {
        let (model, calls) = model(vec![call("effect", json!({})), text("final")]);
        let effects = Arc::new(AtomicUsize::new(0));
        let counted = effects.clone();
        let run = Harness::new(model).run("input").tool(
            tool("effect", "effect", move |_: Value| {
                counted.fetch_add(1, Ordering::SeqCst);
                async { Ok("done") }
            })
            .unwrap(),
        );
        let (run, saved) = fault(run, HistoryOperation::ActionResolution);
        if automatic {
            let incomplete = run.await.unwrap_err();
            storage(&incomplete.reason);
            failed(
                &incomplete.conversation,
                HistoryOperation::ActionResolution,
                EffectState::Returned,
            );
            let mut memory = incomplete.conversation.into_memory().unwrap();
            memory.adopt_pending_write().unwrap();
            assert_eq!(memory.pending_count(), 0);
            assert_eq!(memory.detached_failures().len(), 1);
            assert!(matches!(
                incomplete.report.outcome,
                Some(RunOutcome::Incomplete { .. })
            ));
        } else {
            let mut session = run.manual();
            let action = session.call_model().await.unwrap().actions.remove(0);
            storage(&session.execute(action).await.unwrap_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        let operations: Vec<_> = saved
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.event.operation())
            .collect();
        assert_eq!(
            operations,
            [
                HistoryOperation::Open,
                HistoryOperation::Response,
                HistoryOperation::ToolStart,
                HistoryOperation::ActionResolution
            ]
        );
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Answer {
    value: String,
}

#[tokio::test]
async fn completion_failure_never_accepts_or_reconstructs_application_value() {
    let (model, _) = model(vec![call("submit", json!({"value":"candidate"}))]);
    let (run, _) = fault(
        Harness::new(model).run("input").structured::<Answer>(),
        HistoryOperation::SubmissionCompletion,
    );
    let incomplete = run
        .inspect(async |candidate: Answer| {
            // No Clone/Serialize bound is required by storage.
            Ok::<_, ()>(
                Decision::accept(std::rc::Rc::new(candidate.value))
                    .retain(std::rc::Rc::new("unfinished".into())),
            )
        })
        .await
        .unwrap_err();
    storage(&incomplete.reason);
    assert_eq!(
        incomplete.retained.as_deref().map(|v| v.as_str()),
        Some("unfinished")
    );
    failed(
        &incomplete.conversation,
        HistoryOperation::SubmissionCompletion,
        EffectState::NoEffect,
    );
    assert_eq!(incomplete.conversation.pending_count(), 1);
    assert!(matches!(
        &incomplete.conversation.pending_write().unwrap().event,
        HistoryEvent::SubmissionCompleted {
            kind: SubmissionCompletion::Application,
            ..
        }
    ));
    let mut memory = incomplete.conversation.into_memory().unwrap();
    assert_eq!(
        memory.adopt_pending_write().unwrap_err(),
        ConversationError::CannotAdoptWrite
    );
    assert!(memory.pending_write().is_some());
    memory.discard_pending_write().unwrap();
    let action = memory.pending_actions().unwrap().actions.remove(0);
    assert!(
        memory.accept_submission(&action, "bad").is_err(),
        "validation is not restored"
    );
    assert!(matches!(
        memory.submission::<Answer>(&action).unwrap(),
        Submission::Ready(_)
    ));
    assert_eq!(
        memory
            .accept_submission(&action, "fresh explicit choice")
            .unwrap(),
        "fresh explicit choice"
    );
    assert_eq!(memory.detached_failures().len(), 1);
}

#[tokio::test]
async fn direct_schema_completion_is_distinct_and_atomic() {
    let (model, _) = model(vec![call("submit", json!({"value":"typed"}))]);
    let (run, saved) = fault(
        Harness::new(model).run("input").structured::<Answer>(),
        HistoryOperation::Stop,
    );
    assert_eq!(run.await.unwrap().value, "typed");
    let records = saved.lock().unwrap();
    assert_eq!(records.len(), 3);
    assert!(matches!(
        &records[2].event,
        HistoryEvent::SubmissionCompleted {
            kind: SubmissionCompletion::Schema,
            ..
        }
    ));
    let HistoryEvent::Opened { mode, .. } = &records[0].event else {
        panic!()
    };
    assert!(matches!(mode, super::super::HistoryMode::Structured(_)));
}

#[tokio::test]
async fn manual_detach_keeps_borrowed_tool_and_budget_but_separates_reports() {
    let effects = AtomicUsize::new(0);
    let budget = DefaultBudget::default();
    let (model, calls) = model(vec![call("effect", json!({})), text("done")]);
    let mut run = Harness::new(model).run("input").budget(&budget).tool(
        tool("effect", "effect", |_: Value| {
            effects.fetch_add(1, Ordering::SeqCst);
            async { Ok("result") }
        })
        .unwrap(),
    );
    let saved = Arc::new(Mutex::new(Vec::new()));
    run.core.conversation.attach_history(Box::new(FaultStore {
        records: saved.clone(),
        fail: HistoryOperation::ActionResolution,
        persist_failed: false,
        cancel: None,
    }));
    let mut session = run.manual();
    let action = session.call_model().await.unwrap().actions.remove(0);
    storage(&session.execute(action).await.unwrap_err());
    let old = session.report();
    let old_snapshot = old.snapshot();
    let writes_before = saved.lock().unwrap().len();
    let mut memory = session.into_memory().unwrap();
    assert!(
        memory.call_model().await.is_err(),
        "must choose pending candidate first"
    );
    memory.adopt_pending_write().unwrap();
    memory.call_model().await.unwrap();
    assert_eq!(memory.finish().unwrap(), "done");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(saved.lock().unwrap().len(), writes_before);
    assert_eq!(old.snapshot(), old_snapshot);
    assert_eq!(memory.report().snapshot().attempts.len(), 1);
    let actions = memory.report().snapshot().actions;
    assert_eq!(actions.len(), 1);
    assert!(
        !actions[0].started,
        "adoption does not invent a new invocation"
    );
    assert_eq!(
        actions[0].outcome,
        Some(super::super::ActionOutcome::Resolved)
    );
    assert_eq!(memory.conversation().detached_failures().len(), 1);
}

#[tokio::test]
async fn rejection_user_repair_and_finish_failures_are_storage_stops() {
    // Every synchronous public path must normalize storage failures too.
    let (m, _) = model(vec![call("unknown", json!({}))]);
    let (run, _) = fault(
        Harness::new(m).run("input"),
        HistoryOperation::ActionResolution,
    );
    let mut session = run.manual();
    let action = session.call_model().await.unwrap().actions.remove(0);
    storage(&session.reject(action, "no").unwrap_err());
    let (m, _) = model(vec![]);
    let (run, _) = fault(Harness::new(m).run("input"), HistoryOperation::User);
    let mut session = run.manual();
    storage(&session.feedback("new feedback").unwrap_err());
    let (m, _) = model(vec![text("not a submission")]);
    let (run, _) = fault(
        Harness::new(m).run("input").structured::<Answer>(),
        HistoryOperation::User,
    );
    let incomplete = run.await.unwrap_err();
    storage(&incomplete.reason);
    let (m, _) = model(vec![call("submit", json!({"wrong":true}))]);
    let (run, _) = fault(
        Harness::new(m).run("input").structured::<Answer>(),
        HistoryOperation::ActionResolution,
    );
    storage(&run.await.unwrap_err().reason);
    let (m, _) = model(vec![text("done")]);
    let (run, _) = fault(Harness::new(m).run("input"), HistoryOperation::Finish);
    storage(&run.await.unwrap_err().reason);
    let (m, _) = model(vec![]);
    let (run, _) = fault(Harness::new(m).run("input"), HistoryOperation::Stop);
    let mut session = run.manual();
    storage(&session.stop("application stop"));
}

#[test]
fn low_level_history_never_invents_initial_messages_or_tool_starts_for_submit() {
    let mut empty = Conversation::default();
    empty.record_response(text("one"), vec![]).unwrap();
    assert!(matches!(
        &empty.history()[0].event,
        HistoryEvent::Opened { initial: None, .. }
    ));
    assert!(matches!(
        &empty.history()[1].event,
        HistoryEvent::Response {
            message: 0,
            origin: None,
            ..
        }
    ));
    let mut conversation = Conversation::new("");
    let action = conversation
        .record_response(call("submit", json!({"value":"x"})), vec![])
        .unwrap()
        .actions
        .remove(0);
    assert!(matches!(
        &conversation.history()[0].event,
        HistoryEvent::Opened {
            initial: Some(_),
            ..
        }
    ));
    let count = conversation.history().len();
    assert_eq!(
        conversation.start(&action).unwrap_err(),
        ConversationError::SubmissionStart
    );
    assert_eq!(conversation.history().len(), count);
    assert_eq!(
        conversation.action(&action).unwrap().status,
        super::super::ActionStatus::Proposed
    );
}

#[tokio::test]
async fn prevented_start_can_be_explicitly_discarded_then_invoked_in_memory() {
    let (m, _) = model(vec![call("effect", json!({}))]);
    let effects = Arc::new(AtomicUsize::new(0));
    let counted = effects.clone();
    let run = Harness::new(m).run("input").tool(
        tool("effect", "effect", move |_: Value| {
            counted.fetch_add(1, Ordering::SeqCst);
            async { Ok("done") }
        })
        .unwrap(),
    );
    let (run, _) = fault(run, HistoryOperation::ToolStart);
    let mut session = run.manual();
    let action = session.call_model().await.unwrap().actions.remove(0);
    storage(&session.execute(action).await.unwrap_err());
    let mut memory = session.into_memory().unwrap();
    assert!(memory.adopt_pending_write().is_err());
    memory.discard_pending_write().unwrap();
    let action = memory.pending_actions().unwrap().actions.remove(0);
    memory.execute(action).await.unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let report = memory.report().snapshot();
    assert_eq!(report.actions.len(), 1);
    assert!(report.actions[0].started);
    assert_eq!(
        report.actions[0].outcome,
        Some(super::super::ActionOutcome::Resolved)
    );
}

#[derive(Debug)]
struct CancelStart(CancellationToken);
impl HistoryWriter for CancelStart {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        if matches!(record.event, HistoryEvent::ToolStarted { .. }) {
            self.0.cancel();
        }
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_after_start_commit_prevents_dispatch_but_keeps_interrupted_state() {
    let (m, _) = model(vec![call("effect", json!({}))]);
    let token = CancellationToken::new();
    let mut run = Harness::new(m)
        .run("input")
        .cancellation(token.clone())
        .tool(
            tool("effect", "effect", |_: Value| async {
                panic!("cancelled before effect");
                #[allow(unreachable_code)]
                Ok("bad")
            })
            .unwrap(),
        );
    run.core
        .conversation
        .attach_history(Box::new(CancelStart(token)));
    let mut session = run.manual();
    let action = session.call_model().await.unwrap().actions.remove(0);
    assert!(matches!(
        session.execute(action).await.unwrap_err(),
        ManualError::Stop(ExecutionStop::Cancelled(_))
    ));
    let action = &session.conversation().responses()[0].actions()[0];
    assert_eq!(action.status(), super::super::ActionStatus::Started);
    assert!(action.resolution().is_none());
    assert!(
        session
            .conversation()
            .history()
            .iter()
            .any(|r| matches!(r.event, HistoryEvent::ToolStarted { .. }))
    );
}

#[derive(Debug)]
struct CannotSerialize;
impl HistoryWriter for CannotSerialize {
    fn commit(&mut self, _: &HistoryRecord) -> Result<(), HistoryWriteError> {
        Err(HistoryWriteError {
            phase: HistoryWritePhase::Serialize,
            cause: "cannot encode".into(),
        })
    }
}

#[tokio::test]
async fn serialization_failure_is_known_unwritten_and_open_can_be_adopted_explicitly() {
    let (m, calls) = model(vec![text("done")]);
    let mut run = Harness::new(m)
        .run("input")
        .instructions("saved instructions");
    run.core
        .conversation
        .attach_history(Box::new(CannotSerialize));
    let mut session = run.manual();
    storage(&session.call_model().await.unwrap_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        session
            .conversation()
            .history_failure()
            .unwrap()
            .persistence,
        PersistenceOutcome::NotWritten
    );
    let mut memory = session.into_memory().unwrap();
    memory.adopt_pending_write().unwrap();
    memory.call_model().await.unwrap();
    assert_eq!(memory.finish().unwrap(), "done");
    assert!(
        matches!(&memory.conversation().history()[0].event, HistoryEvent::Opened { instructions, .. } if instructions == "saved instructions")
    );
}

#[tokio::test]
async fn complete_response_candidate_can_be_adopted_without_another_model_call() {
    let (m, calls) = model(vec![text("known response")]);
    let (run, _) = fault(Harness::new(m).run("input"), HistoryOperation::Response);
    let mut session = run.manual();
    storage(&session.call_model().await.unwrap_err());
    let mut memory = session.into_memory().unwrap();
    assert!(matches!(
        memory.discard_pending_write(),
        Err(ManualError::Conversation(
            ConversationError::CannotDiscardWrite
        ))
    ));
    memory.adopt_pending_write().unwrap();
    assert_eq!(memory.finish().unwrap(), "known response");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        memory.conversation().responses()[0].origin(),
        Some("fake/model")
    );
}

#[tokio::test]
async fn failed_start_remains_storage_reason_even_if_writer_also_cancels() {
    let (m, _) = model(vec![call("effect", json!({}))]);
    let token = CancellationToken::new();
    let mut run = Harness::new(m)
        .run("input")
        .cancellation(token.clone())
        .tool(
            tool("effect", "effect", |_: Value| async {
                panic!("failed before dispatch");
                #[allow(unreachable_code)]
                Ok("bad")
            })
            .unwrap(),
        );
    run.core.conversation.attach_history(Box::new(FaultStore {
        records: Arc::default(),
        fail: HistoryOperation::ToolStart,
        persist_failed: true,
        cancel: Some(token),
    }));
    let mut session = run.manual();
    let action = session.call_model().await.unwrap().actions.remove(0);
    storage(&session.execute(action).await.unwrap_err());
    failed(
        session.conversation(),
        HistoryOperation::ToolStart,
        EffectState::Prevented,
    );
}

#[test]
fn low_level_explicit_stop_reports_failed_required_write() {
    let mut conversation = Conversation::new("input");
    conversation.attach_history(Box::new(FaultStore {
        records: Arc::default(),
        fail: HistoryOperation::Stop,
        persist_failed: false,
        cancel: None,
    }));
    assert!(matches!(
        conversation.stop(),
        Err(ConversationError::Storage(_))
    ));
    failed(&conversation, HistoryOperation::Stop, EffectState::NoEffect);
}

#[tokio::test]
async fn immediate_stop_persists_instructions_and_structured_mode_without_calls() {
    let (m, calls) = model(vec![]);
    let mut session = Harness::new(m)
        .run("original input")
        .instructions("original instructions")
        .structured::<Answer>()
        .manual();
    assert!(matches!(
        session.stop("stop before dispatch"),
        ManualError::Stop(ExecutionStop::Stopped(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let HistoryEvent::Opened {
        initial,
        instructions,
        mode,
    } = &session.conversation().history()[0].event
    else {
        panic!("missing open")
    };
    assert_eq!(instructions, "original instructions");
    assert!(initial.is_some());
    assert_eq!(
        mode,
        &super::super::HistoryMode::Structured(submission_tool::<Answer>().parameters)
    );
    assert!(matches!(
        session.conversation().history()[1].event,
        HistoryEvent::Stopped { .. }
    ));
}

#[derive(Debug)]
struct CancelCompletion {
    operation: HistoryOperation,
    cancellation: CancellationToken,
}
impl HistoryWriter for CancelCompletion {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        if record.event.operation() == self.operation {
            self.cancellation.cancel();
        }
        Ok(())
    }
}
fn cancel_on_completion<Mode>(
    mut run: Run<'static, Mode>,
    operation: HistoryOperation,
) -> Run<'static, Mode> {
    let cancellation = CancellationToken::new();
    run = run.cancellation(cancellation.clone());
    run.core
        .conversation
        .attach_history(Box::new(CancelCompletion {
            operation,
            cancellation,
        }));
    run
}
fn cancelled_after_durable_completion<A>(incomplete: &Incomplete<A>, report: &ReportHandle) {
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::Cancelled(_))
    ));
    assert_eq!(
        incomplete.conversation.status(),
        super::super::ConversationStatus::Finished
    );
    assert!(incomplete.conversation.history_failure().is_none());
    assert!(incomplete.conversation.pending_write().is_none());
    assert!(matches!(
        incomplete.report.outcome,
        Some(RunOutcome::Incomplete {
            kind: super::super::FailureKind::Cancelled,
            ..
        })
    ));
    assert_eq!(*incomplete.report, report.snapshot());
}

#[tokio::test]
async fn cancellation_during_text_completion_keeps_terminal_history_but_returns_incomplete() {
    let (m, calls) = model(vec![text("answer")]);
    let run = cancel_on_completion(Harness::new(m).run("input"), HistoryOperation::Finish);
    let report = run.report();
    let incomplete = run.await.unwrap_err();
    cancelled_after_durable_completion(&incomplete, &report);
    assert!(matches!(
        incomplete.conversation.history().last().unwrap().event,
        HistoryEvent::Finished { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        incomplete.conversation.into_memory().unwrap_err(),
        ConversationError::NotStorageStopped
    );
}

#[tokio::test]
async fn cancellation_during_schema_completion_never_returns_the_typed_value() {
    let (m, calls) = model(vec![call("submit", json!({"value":"typed"}))]);
    let run = cancel_on_completion(
        Harness::new(m).run("input").structured::<Answer>(),
        HistoryOperation::SubmissionCompletion,
    );
    let report = run.report();
    let incomplete = run.await.unwrap_err();
    cancelled_after_durable_completion(&incomplete, &report);
    assert!(matches!(
        incomplete.conversation.history().last().unwrap().event,
        HistoryEvent::SubmissionCompleted {
            kind: SubmissionCompletion::Schema,
            ..
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_application_completion_never_promotes_retained_progress() {
    let (m, calls) = model(vec![call("submit", json!({"value":"candidate"}))]);
    let run = cancel_on_completion(
        Harness::new(m).run("input").structured::<Answer>(),
        HistoryOperation::SubmissionCompletion,
    );
    let report = run.report();
    let mut progress = run.subscribe();
    let incomplete = run
        .inspect(async |candidate: Answer| {
            Ok::<_, ()>(Decision::accept(candidate.value).retain("unfinished".into()))
        })
        .await
        .unwrap_err();
    cancelled_after_durable_completion(&incomplete, &report);
    assert_eq!(incomplete.retained.as_deref(), Some("unfinished"));
    assert!(matches!(
        incomplete.conversation.history().last().unwrap().event,
        HistoryEvent::SubmissionCompleted {
            kind: SubmissionCompletion::Application,
            ..
        }
    ));
    while let Some(event) = progress.recv().await {
        assert!(!matches!(
            event,
            Progress::Refinement {
                stage: RefinementStage::Accepted,
                ..
            }
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_completion_write_takes_precedence_over_simultaneous_cancellation() {
    let (m, _) = model(vec![call("submit", json!({"value":"candidate"}))]);
    let token = CancellationToken::new();
    let mut run = Harness::new(m)
        .run("input")
        .structured::<Answer>()
        .cancellation(token.clone());
    run.core.conversation.attach_history(Box::new(FaultStore {
        records: Arc::default(),
        fail: HistoryOperation::SubmissionCompletion,
        persist_failed: true,
        cancel: Some(token),
    }));
    let incomplete = run.await.unwrap_err();
    storage(&incomplete.reason);
    failed(
        &incomplete.conversation,
        HistoryOperation::SubmissionCompletion,
        EffectState::NoEffect,
    );
    assert_eq!(incomplete.conversation.pending_count(), 1);
    assert!(matches!(
        incomplete.report.outcome,
        Some(RunOutcome::Incomplete {
            kind: super::super::FailureKind::Storage,
            ..
        })
    ));
}

#[tokio::test]
async fn resumed_tool_result_failure_retains_known_effect_in_normal_incomplete() {
    let (model, calls) = model(vec![call("effect", json!({}))]);
    let harness = Harness::new(model);
    let effects = Arc::new(AtomicUsize::new(0));
    let make_tool = || {
        let effects = effects.clone();
        tool("effect", "count", move |_: Value| {
            effects.fetch_add(1, Ordering::SeqCst);
            async { Ok("completed effect") }
        })
        .unwrap()
    };
    let (run, _) = fault(
        harness.run("input").tool(make_tool()),
        HistoryOperation::ActionResolution,
    );
    let mut session = run.manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    let mut conversation = session.into_conversation();
    conversation.start(&action).unwrap();
    let incomplete = harness
        .resume(conversation)
        .tool(make_tool())
        .await
        .unwrap_err();
    storage(&incomplete.reason);
    failed(
        &incomplete.conversation,
        HistoryOperation::ActionResolution,
        EffectState::Returned,
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut memory = incomplete.conversation.into_memory().unwrap();
    memory.adopt_pending_write().unwrap();
    assert_eq!(memory.pending_count(), 0);
}

#[tokio::test]
async fn resumed_inspector_completion_failure_never_returns_accepted_value() {
    let (model, calls) = model(vec![call("submit", json!({"value":"saved"}))]);
    let harness = Harness::new(model);
    let (run, _) = fault(
        harness.run("input").structured::<Answer>(),
        HistoryOperation::SubmissionCompletion,
    );
    let mut session = run.manual();
    session.call_model().await.unwrap();
    let incomplete = harness
        .resume(session.into_conversation())
        .structured::<Answer>()
        .inspect(async |_: Answer| {
            Ok::<_, String>(Decision::accept("accepted").retain("unfinished"))
        })
        .await
        .unwrap_err();
    storage(&incomplete.reason);
    assert_eq!(incomplete.retained, Some("unfinished"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(incomplete.conversation.pending_count(), 1);
    assert!(!matches!(
        incomplete.conversation.status(),
        super::super::ConversationStatus::Finished
    ));
}
