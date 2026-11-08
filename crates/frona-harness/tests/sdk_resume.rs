#![cfg(feature = "jsonl")]
use async_trait::async_trait;
use frona_harness::{
    inference::{InferenceError, provider::StreamToken},
    *,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction, UserContent},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::mpsc;

struct Script {
    outputs: Mutex<VecDeque<ModelOutput>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
#[async_trait]
impl ModelAdapter for Script {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model invocation"))
    }
}
fn model(outputs: Vec<ModelOutput>) -> (Model<'static>, Arc<Mutex<Vec<ModelRequest>>>) {
    named_model("local", outputs)
}
fn named_model(
    provider: &str,
    outputs: Vec<ModelOutput>,
) -> (Model<'static>, Arc<Mutex<Vec<ModelRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        Model::from_adapter(
            provider,
            "resume",
            Script {
                outputs: Mutex::new(outputs.into()),
                requests: requests.clone(),
            },
        )
        .unwrap(),
        requests,
    )
}
fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(content, UsageReport::Unavailable)
}
fn text(s: &str) -> ModelOutput {
    output(vec![AssistantContent::text(s)])
}
fn call(name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        "call",
        ToolFunction::new(name.into(), arguments),
    ))
}
fn submission(value: u32) -> ModelOutput {
    output(vec![call("submit", json!({"value":value}))])
}
#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
struct Answer {
    value: u32,
}
#[derive(Debug, Serialize, Deserialize)]
struct AppState {
    inspections: usize,
    candidate: u32,
}
fn work(effects: Arc<AtomicUsize>) -> impl Tool + use<> {
    tool("work", "count", move |_: Value| {
        let effects = effects.clone();
        async move {
            effects.fetch_add(1, Ordering::SeqCst);
            Ok("worked")
        }
    })
    .unwrap()
}

#[tokio::test]
async fn saved_refinement_restores_borrowed_inspector_without_accepting_retained_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refine.jsonl");
    let (model, requests) = model(vec![submission(7)]);
    let harness = Harness::new(model);
    let mut state = AppState {
        inspections: 0,
        candidate: 0,
    };
    let incomplete = harness
        .run("question")
        .jsonl(Jsonl::create(&path).unwrap())
        .instructions("saved instructions")
        .structured::<Answer>()
        .inspect(async |answer| {
            state.inspections += 1;
            state.candidate = answer.value;
            Ok::<_, String>(
                Decision::<String>::stop("checkpoint boundary").retain("unfinished".into()),
            )
        })
        .await
        .unwrap_err();
    assert_eq!(incomplete.retained.as_deref(), Some("unfinished"));
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Stopped
    );
    let mut conversation = *incomplete.conversation;
    conversation.save_checkpoint("app", 1, &state).unwrap();
    drop(conversation);
    let loaded = Jsonl::open(&path).unwrap();
    let mut restored: AppState = loaded.load_checkpoint("app", 1).unwrap().unwrap().payload;
    assert_eq!(restored.inspections, 1);
    let accepted = harness
        .resume(loaded)
        .structured::<Answer>()
        .inspect(async |answer| {
            restored.inspections += 1;
            Ok::<_, String>(Decision::accept(format!(
                "accepted {} after {}",
                answer.value, restored.inspections
            )))
        })
        .await
        .unwrap();
    assert_eq!(accepted, "accepted 7 after 2");
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "pending submit inspected before another model"
    );
    let saved = Jsonl::open(&path).unwrap();
    assert_eq!(saved.conversation().status(), ConversationStatus::Finished);
    assert!(saved.checkpoint_stale());
    assert!(matches!(
        saved.conversation().history().last().unwrap().event,
        HistoryEvent::SubmissionCompleted {
            kind: SubmissionCompletion::Application,
            ..
        }
    ));
}

#[tokio::test]
async fn manual_resume_rejects_unstarted_tool_and_accepts_submission_without_model_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manual.jsonl");
    let (model, requests) = model(vec![output(vec![
        call("work", json!({})),
        call("submit", json!({"value":9})),
    ])]);
    let harness = Harness::new(model);
    let effects = Arc::new(AtomicUsize::new(0));
    let mut original = harness
        .run("input")
        .structured::<Answer>()
        .instructions("system")
        .jsonl(Jsonl::create(&path).unwrap())
        .tool(work(effects.clone()))
        .manual();
    original.call_model().await.unwrap();
    original.stop("pause");
    original
        .save_checkpoint("app", 1, &json!({"paused":true}))
        .unwrap();
    drop(original);
    let before = std::fs::read(&path).unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    let mut resumed = harness
        .resume(loaded)
        .structured::<Answer>()
        .tool(work(effects.clone()))
        .manual();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(matches!(
        resumed.call_model().await,
        Err(ManualError::Resume(ResumeError::PreparationRequired))
    ));
    assert!(
        resumed
            .load_checkpoint::<Value>("app", 1)
            .unwrap()
            .is_some()
    );
    let mut pending = resumed.recover().await.unwrap().actions.into_iter();
    resumed
        .reject(pending.next().unwrap(), "caller rejects")
        .unwrap();
    let action = pending.next().unwrap();
    assert!(matches!(
        resumed.submission(&action),
        Ok(Submission::Ready(Answer { value: 9 }))
    ));
    assert_eq!(
        resumed.accept(action, "application accepted").unwrap(),
        "application accepted"
    );
    resumed
        .save_checkpoint("app", 1, &json!({"complete":true}))
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        Jsonl::open(&path).unwrap().conversation().status(),
        ConversationStatus::Finished
    );
}

#[tokio::test]
async fn automatic_resume_recovers_interrupted_tool_and_is_owned_send() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retry.jsonl");
    let (model, requests) = model(vec![output(vec![call("work", json!({}))]), text("done")]);
    let harness = Harness::new(model);
    let effects = Arc::new(AtomicUsize::new(0));
    let mut session = harness
        .run("input")
        .jsonl(Jsonl::create(&path).unwrap())
        .tool(work(effects.clone()))
        .manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    let mut conversation = session.into_conversation();
    conversation.start(&action).unwrap();
    drop(conversation);
    let budget = Arc::new(DefaultBudget::default());
    let run = harness
        .resume(Jsonl::open(&path).unwrap())
        .tool(work(effects.clone()))
        .budget(budget.clone());
    let report = run.report();
    let answer = tokio::spawn(async move { run.await })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer, "done");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(
        requests.lock().unwrap()[1].messages.len(),
        3,
        "text continuation adds no submission repair"
    );
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(
        report.snapshot().requests.len(),
        1,
        "no historical requests fabricated"
    );
    let saved = Jsonl::open(&path).unwrap();
    assert_eq!(
        saved.conversation().responses()[0].actions()[0].invocation_ordinal(),
        2
    );
}

#[tokio::test]
async fn saved_final_text_is_used_once_but_later_feedback_requires_a_new_response() {
    for feedback in [false, true] {
        let (model, requests) = model(vec![text("old"), text("new")]);
        let harness = Harness::new(model);
        let mut session = harness.run("input").manual();
        session.call_model().await.unwrap();
        if feedback {
            session.feedback("revise").unwrap();
        }
        let conversation = session.into_conversation();
        let answer = harness.resume(conversation).await.unwrap();
        assert_eq!(answer, if feedback { "new" } else { "old" });
        assert_eq!(requests.lock().unwrap().len(), if feedback { 2 } else { 1 });
    }
}

#[tokio::test]
async fn resolved_submission_in_a_mixed_batch_does_not_trigger_missing_submission_repair() {
    for started in [false, true] {
        let (model, requests) = model(vec![
            output(vec![
                call("submit", json!({"wrong":true})),
                call("work", json!({})),
            ]),
            submission(4),
        ]);
        let harness = Harness::new(model);
        let effects = Arc::new(AtomicUsize::new(0));
        let mut session = harness
            .run("input")
            .structured::<Answer>()
            .tool(work(effects.clone()))
            .manual();
        let mut actions = session.call_model().await.unwrap().actions.into_iter();
        assert!(matches!(
            session.submission(&actions.next().unwrap()).unwrap(),
            Submission::Repaired { .. }
        ));
        let action = actions.next().unwrap();
        let mut conversation = session.into_conversation();
        if started {
            conversation.start(&action).unwrap();
        }
        let answer = harness
            .resume(conversation)
            .structured::<Answer>()
            .tool(work(effects.clone()))
            .await
            .unwrap();
        assert_eq!(answer.value, 4);
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert_eq!(
            requests.lock().unwrap()[1].messages.len(),
            3,
            "resolved submit already has paired repair"
        );
    }
}

#[tokio::test]
async fn resumed_tool_only_batches_receive_missing_submission_feedback_exactly_once() {
    for boundary in ["started", "resolved", "repaired"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool-only.jsonl");
        let (model, requests) = model(vec![output(vec![call("work", json!({}))]), submission(5)]);
        let harness = Harness::new(model);
        let effects = Arc::new(AtomicUsize::new(0));
        let mut session = harness
            .run("input")
            .jsonl(Jsonl::create(&path).unwrap())
            .structured::<Answer>()
            .tool(work(effects.clone()))
            .manual();
        let action = session.call_model().await.unwrap().actions.pop().unwrap();
        if boundary == "started" {
            let mut conversation = session.into_conversation();
            conversation.start(&action).unwrap();
        } else {
            session.execute(action).await.unwrap();
            if boundary == "repaired" {
                session.repair_missing_submission().unwrap();
            }
            drop(session);
        }
        let answer = harness
            .resume(Jsonl::open(&path).unwrap())
            .structured::<Answer>()
            .tool(work(effects.clone()))
            .await
            .unwrap();
        assert_eq!(answer.value, 5);
        assert_eq!(
            effects.load(Ordering::SeqCst),
            1,
            "{boundary}: tools never replay after resolution"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].messages.len(),
            4,
            "{boundary}: one result batch and exactly one missing-submit repair"
        );
        let rig_core::completion::Message::User { content } = &requests[1].messages[3] else {
            panic!("missing-submission feedback must be a user message");
        };
        assert!(
            matches!(&content[0], UserContent::Text(text) if text.text.contains("without a recorded submission"))
        );
        let loaded = Jsonl::open(&path).unwrap();
        assert_eq!(
            loaded.conversation().responses()[0].actions()[0].invocation_ordinal(),
            if boundary == "started" { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn saved_missing_submission_prose_is_repaired_only_when_not_already_handled() {
    for repaired in [false, true] {
        let (model, requests) = model(vec![text("prose"), submission(4)]);
        let harness = Harness::new(model);
        let mut session = harness.run("input").structured::<Answer>().manual();
        session.call_model().await.unwrap();
        if repaired {
            session.repair_missing_submission().unwrap();
        }
        assert_eq!(
            harness
                .resume(session.into_conversation())
                .structured::<Answer>()
                .await
                .unwrap()
                .value,
            4
        );
        assert_eq!(requests.lock().unwrap()[1].messages.len(), 3);
    }
}

#[tokio::test]
async fn schema_preflight_preserves_deferred_file_and_can_be_reconfigured() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schema.jsonl");
    let (model, requests) = model(vec![submission(3)]);
    let harness = Harness::new(model);
    let mut session = harness
        .run("input")
        .structured::<Answer>()
        .jsonl(Jsonl::create(&path).unwrap())
        .manual();
    session.call_model().await.unwrap();
    drop(session);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"tail")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    let records = encode_jsonl(loaded.conversation().history()).unwrap();
    let incomplete = harness.resume(loaded).await.unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Resume(ResumeError::ModeMismatch)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        encode_jsonl(incomplete.conversation.history()).unwrap(),
        records,
        "no fake memory stop"
    );
    let mut conversation = *incomplete.conversation;
    assert!(matches!(
        conversation.push_user(vec![UserContent::text("bad")]),
        Err(ConversationError::StorageNotReady) | Err(ConversationError::Terminal)
    ));
    assert_eq!(
        harness
            .resume(conversation)
            .structured::<Answer>()
            .await
            .unwrap()
            .value,
        3
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(Jsonl::open(&path).unwrap().incomplete_tail().is_none());
}

#[tokio::test]
async fn changed_file_returns_storage_incomplete_and_never_silently_uses_memory() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("changed.jsonl");
    let (model, requests) = model(vec![text("saved")]);
    let harness = Harness::new(model);
    let mut session = harness
        .run("input")
        .jsonl(Jsonl::create(&path).unwrap())
        .manual();
    session.call_model().await.unwrap();
    drop(session);
    let loaded = Jsonl::open(&path).unwrap();
    let records = encode_jsonl(loaded.conversation().history()).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let changed = std::fs::read(&path).unwrap();
    let incomplete = harness.resume(loaded).await.unwrap_err();
    let ManualError::Stop(ExecutionStop::Storage(error)) = &incomplete.reason else {
        panic!("typed storage")
    };
    let failure = error.downcast_ref::<HistoryFailure>().unwrap();
    assert_eq!(failure.operation, HistoryOperation::Resume);
    assert_eq!(failure.phase, HistoryWritePhase::Revalidate);
    assert_eq!(failure.persistence, PersistenceOutcome::NotWritten);
    assert!(incomplete.conversation.pending_write().is_none());
    assert_eq!(
        encode_jsonl(incomplete.conversation.history()).unwrap(),
        records
    );
    let again = harness.resume(*incomplete.conversation).await.unwrap_err();
    assert!(matches!(
        again.reason,
        ManualError::Stop(ExecutionStop::Storage(_))
    ));
    let memory = again.conversation.into_memory().unwrap();
    assert_eq!(harness.resume(memory).await.unwrap(), "saved");
    assert_eq!(std::fs::read(&path).unwrap(), changed);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn completed_marker_never_runs_inspector_or_prepares_file() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("complete.jsonl");
    let (model, requests) = model(vec![submission(3)]);
    let harness = Harness::new(model);
    harness
        .run("input")
        .jsonl(Jsonl::create(&path).unwrap())
        .structured::<Answer>()
        .await
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"tail")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut inspections = 0;
    let incomplete = harness
        .resume(Jsonl::open(&path).unwrap())
        .structured::<Answer>()
        .inspect(async |_: Answer| {
            inspections += 1;
            Ok::<_, String>(Decision::accept("must not happen"))
        })
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Dispatch(ToolDispatchError::Recovery(RecoveryError::AlreadyCompleted))
    ));
    assert_eq!(inspections, 0);
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        incomplete.conversation.status(),
        ConversationStatus::Finished
    );
}

#[tokio::test]
async fn recovery_budget_refusal_returns_the_normal_incomplete_shape() {
    let (model, requests) = model(vec![output(vec![call("work", json!({}))])]);
    let harness = Harness::new(model);
    let effects = Arc::new(AtomicUsize::new(0));
    let mut session = harness.run("input").tool(work(effects.clone())).manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    let mut conversation = session.into_conversation();
    conversation.start(&action).unwrap();
    let budget = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(0),
        ..Default::default()
    });
    let incomplete = harness
        .resume(conversation)
        .tool(work(effects.clone()))
        .budget(budget)
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Stop(ExecutionStop::BudgetExhausted(_))
    ));
    assert_eq!(
        incomplete.conversation.responses()[0].actions()[0].status(),
        ActionStatus::Started
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn incompatible_model_is_rejected_before_file_repair_or_interrupted_tool_effect() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compatibility.jsonl");
    let mut proposal = match call("work", json!({})) {
        AssistantContent::ToolCall(call) => call,
        _ => unreachable!(),
    };
    proposal.signature = Some("provider signature".into());
    let (first, _) = model(vec![output(vec![AssistantContent::ToolCall(proposal)])]);
    let effects = Arc::new(AtomicUsize::new(0));
    let mut session = Harness::new(first)
        .run("input")
        .jsonl(Jsonl::create(&path).unwrap())
        .tool(work(effects.clone()))
        .manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    let mut conversation = session.into_conversation();
    conversation.start(&action).unwrap();
    drop(conversation);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"tail")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let (other, requests) = named_model("other", vec![]);
    let incomplete = Harness::new(other)
        .resume(Jsonl::open(&path).unwrap())
        .tool(work(effects.clone()))
        .await
        .unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Incompatible { .. }
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn compacted_view_and_checkpoint_survive_resume_without_resummarization() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compacted.jsonl");
    let mut original = Conversation::new("old input ".repeat(100));
    original
        .record_response(text(&"old answer ".repeat(100)), vec![])
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
        .finish("saved facts", None)
        .unwrap();
    let mut records = original.history().to_vec();
    let id = records[0].conversation;
    records.push(HistoryRecord {
        version: 1,
        conversation: id,
        seq: records.len() as u64,
        event: HistoryEvent::Compacted {
            view: view.clone(),
            summarizer: "local:resume".into(),
            output: text("saved facts"),
        },
    });
    let mut conversation = replay_history(&records).unwrap().conversation;
    conversation
        .save_checkpoint("app", 1, &json!({"state":"restored"}))
        .unwrap();
    let expected = conversation.request_messages().unwrap();
    std::fs::write(&path, encode_jsonl(conversation.history()).unwrap()).unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(
        loaded
            .load_checkpoint::<Value>("app", 1)
            .unwrap()
            .unwrap()
            .payload,
        json!({"state":"restored"})
    );
    let (model, requests) = model(vec![text("final")]);
    let run = Harness::new(model).resume(loaded);
    let report = run.report();
    assert_eq!(run.await.unwrap(), "final");
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(requests.lock().unwrap()[0].messages, expected);
    assert_eq!(report.snapshot().requests.len(), 1);
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(loaded.conversation().compacted_view(), Some(&view));
    assert_eq!(
        &loaded.conversation().messages()[..original.messages().len()],
        original.messages()
    );
    assert!(loaded.checkpoint_stale());
}
