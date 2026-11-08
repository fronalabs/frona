use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use frona_harness::{
    ActionResolution, ActionStatus, BudgetLimits, Capabilities, Conversation, ConversationStatus,
    DefaultBudget, ExecutionControl, ExecutionStop, Harness, HistoryEvent, Model, ModelAdapter,
    ModelOutput, ModelRequest, RecoveryError, ReportHandle, Submission, Tool, ToolCallHook,
    ToolDecision, ToolDispatchError, ToolError, ToolFailure, ToolOutput, ToolRegistry, UsageReport,
    decode_jsonl, encode_jsonl,
    inference::{InferenceError, provider::StreamToken},
    submission_tool, tool, tool_with_context,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn counting(name: &str, calls: Arc<AtomicUsize>) -> impl Tool + use<> {
    tool(name, "Count effects", move |_: Value| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Ok("effect") }
    })
    .unwrap()
}
fn registry(calls: Arc<AtomicUsize>) -> ToolRegistry<'static> {
    let mut tools = ToolRegistry::default();
    tools.register(counting("work", calls)).unwrap();
    tools
}
fn record(
    conversation: &mut Conversation,
    names: &[&str],
    tools: &ToolRegistry<'_>,
) -> Vec<frona_harness::PendingAction> {
    let mut definitions: Vec<_> = tools.definitions().cloned().collect();
    definitions.push(submission_tool::<String>());
    conversation
        .record_response(
            ModelOutput::new(
                names
                    .iter()
                    .map(|name| {
                        AssistantContent::ToolCall(ToolCall::from_wire(
                            "duplicate-id",
                            ToolFunction::new(
                                (*name).into(),
                                if *name == "submit" {
                                    json!("candidate")
                                } else {
                                    json!({})
                                },
                            ),
                        ))
                    })
                    .collect(),
                UsageReport::Unavailable,
            ),
            definitions,
        )
        .unwrap()
        .actions
}
fn interrupted(tools: &ToolRegistry<'_>) -> Conversation {
    let mut conversation = Conversation::new("input");
    let actions = record(&mut conversation, &["work"], tools);
    conversation.start(&actions[0]).unwrap();
    conversation
}
fn replay(conversation: &Conversation) -> Conversation {
    decode_jsonl(&encode_jsonl(conversation.history()).unwrap())
        .unwrap()
        .replay
        .conversation
}
fn starts(conversation: &Conversation) -> Vec<u64> {
    conversation
        .history()
        .iter()
        .filter_map(|r| match r.event {
            HistoryEvent::ToolStarted { ordinal, .. } => Some(ordinal),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn recovery_retries_only_started_calls_and_returns_unstarted_and_submission_handles() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = registry(calls.clone());
    let mut original = Conversation::new("input");
    let handles = record(
        &mut original,
        &["work", "work", "work", "work", "submit"],
        &tools,
    );
    original
        .execute_tool(
            &handles[0],
            &mut tools,
            ExecutionControl::new(&DefaultBudget::default()),
        )
        .await
        .unwrap();
    original
        .reject_tool(&handles[1], ToolOutput::text("denied"))
        .unwrap();
    original.start(&handles[2]).unwrap();
    assert!(matches!(
        original.submission::<String>(&handles[4]).unwrap(),
        Submission::Ready(_)
    ));
    original.stop().unwrap();
    let mut restored = replay(&original);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let budget = DefaultBudget::default();
    let pending = restored
        .recover_tools(&mut tools, ExecutionControl::new(&budget))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(pending.actions.len(), 2);
    assert_eq!(restored.responses()[0].actions()[2].invocation_ordinal(), 2);
    assert_eq!(restored.responses()[0].actions()[0].invocation_ordinal(), 1);
    assert_eq!(
        restored.responses()[0].actions()[1].status(),
        ActionStatus::Rejected
    );
    assert_eq!(
        restored.responses()[0].actions()[3].status(),
        ActionStatus::Proposed
    );
    let mut pending = pending.actions.into_iter();
    let unstarted = pending.next().unwrap();
    let submission = pending.next().unwrap();
    assert!(!restored.action(&submission).unwrap().submission_validated);
    restored
        .reject_tool(&unstarted, ToolOutput::text("caller choice"))
        .unwrap();
    assert!(
        restored
            .accept_submission(&submission, "not validated")
            .is_err()
    );
    assert!(matches!(
        restored.submission::<String>(&submission).unwrap(),
        Submission::Ready(_)
    ));
    assert_eq!(
        restored
            .accept_submission(&submission, "explicit acceptance")
            .unwrap(),
        "explicit acceptance"
    );
    let before = encode_jsonl(restored.history()).unwrap();
    assert!(matches!(
        restored
            .recover_tools(&mut tools, ExecutionControl::new(&budget))
            .await,
        Err(ToolDispatchError::Recovery(RecoveryError::AlreadyCompleted))
    ));
    assert_eq!(encode_jsonl(restored.history()).unwrap(), before);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn effect_without_result_can_repeat_but_saved_ordinary_failure_never_repeats() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    let effects = calls.clone();
    tools
        .register(
            tool("work", "Effect before stop", move |_: Value| {
                let attempt = effects.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(ToolError::Stop(ExecutionStop::Stopped(
                            "crash after effect".into(),
                        )))
                    } else {
                        Err::<String, _>(ToolError::failed("ordinary failure with saved result"))
                    }
                }
            })
            .unwrap(),
        )
        .unwrap();
    let mut original = Conversation::new("input");
    let handles = record(&mut original, &["work"], &tools);
    assert!(matches!(
        original
            .execute_tool(
                &handles[0],
                &mut tools,
                ExecutionControl::new(&DefaultBudget::default())
            )
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::Stopped(_)))
    ));
    let mut restored = replay(&original);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        restored.responses()[0].actions()[0].status(),
        ActionStatus::Started
    );
    assert!(
        restored
            .recover_tools(&mut tools, ExecutionControl::new(&DefaultBudget::default()))
            .await
            .unwrap()
            .actions
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        restored.responses()[0].actions()[0].resolution(),
        Some(ActionResolution::Failed(_))
    ));
    let mut reopened = replay(&restored);
    reopened
        .recover_tools(&mut tools, ExecutionControl::new(&DefaultBudget::default()))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn fresh_budget_refusal_preserves_interrupted_state_and_later_recovery_can_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = registry(calls.clone());
    let mut conversation = interrupted(&tools);
    let exhausted = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(0),
        ..Default::default()
    });
    assert!(matches!(
        conversation
            .recover_tools(&mut tools, ExecutionControl::new(&exhausted))
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(starts(&conversation), vec![1]);
    assert_eq!(conversation.status(), ConversationStatus::Stopped);
    let available = DefaultBudget::default();
    conversation
        .recover_tools(&mut tools, ExecutionControl::new(&available))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(starts(&conversation), vec![1, 2]);
}

struct Policy {
    decision: Result<ToolDecision, &'static str>,
    calls: usize,
    cancel: Option<CancellationToken>,
    pending: bool,
}
#[async_trait]
impl ToolCallHook for Policy {
    async fn decide(&mut self, _: &ToolCall) -> Result<ToolDecision, ToolFailure> {
        self.calls += 1;
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if self.pending {
            std::future::pending::<()>().await;
        }
        self.decision.clone().map_err(Into::into)
    }
}
#[tokio::test]
async fn restored_hook_refusal_failure_and_cancellation_never_resolve_prior_effect() {
    for case in 0..6 {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tools = registry(calls.clone());
        let mut conversation = interrupted(&tools);
        let budget = DefaultBudget::default();
        let cancellation = CancellationToken::new();
        let mut policy = Policy {
            decision: match case {
                0 => Ok(ToolDecision::Reject("no retry".into())),
                1 => Ok(ToolDecision::Stop("wait".into())),
                2 => Err("policy unavailable"),
                _ => Ok(ToolDecision::Execute),
            },
            calls: 0,
            cancel: (case >= 3).then(|| cancellation.clone()),
            pending: case == 4,
        };
        if case == 5 {
            cancellation.cancel();
        }
        let result = conversation
            .recover_tools_with_hook(
                &mut tools,
                ExecutionControl::new(&budget).with_cancellation(&cancellation),
                &mut policy,
            )
            .await;
        match case {
            0 | 1 => assert!(matches!(
                result,
                Err(ToolDispatchError::Stop(ExecutionStop::Stopped(_)))
            )),
            2 => assert!(matches!(result, Err(ToolDispatchError::ToolHook(_)))),
            _ => assert!(matches!(
                result,
                Err(ToolDispatchError::Stop(ExecutionStop::Cancelled(_)))
            )),
        }
        assert_eq!(policy.calls, usize::from(case != 5));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(budget.snapshot().tool_calls, 0);
        assert_eq!(starts(&conversation), vec![1]);
        assert_eq!(
            conversation.responses()[0].actions()[0].status(),
            ActionStatus::Started
        );
        assert!(
            conversation.responses()[0].actions()[0]
                .resolution()
                .is_none()
        );
        let mut allow = Policy {
            decision: Ok(ToolDecision::Execute),
            calls: 0,
            cancel: None,
            pending: false,
        };
        conversation
            .recover_tools_with_hook(&mut tools, ExecutionControl::new(&budget), &mut allow)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(allow.calls, 1);
        assert_eq!(starts(&conversation), vec![1, 2]);
    }
}

#[tokio::test]
async fn all_interrupted_dependencies_are_checked_before_any_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut original_tools = registry(calls.clone());
    original_tools
        .register(counting("missing", calls.clone()))
        .unwrap();
    let mut conversation = Conversation::new("input");
    let handles = record(&mut conversation, &["work", "missing"], &original_tools);
    for action in &handles {
        conversation.start(action).unwrap();
    }
    let mut restored_tools = registry(calls.clone());
    assert!(
        matches!(conversation.recover_tools(&mut restored_tools, ExecutionControl::new(&DefaultBudget::default())).await,
        Err(ToolDispatchError::Recovery(RecoveryError::MissingTool {name})) if name == "missing")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(starts(&conversation), vec![1, 1]);
    assert!(
        conversation.responses()[0]
            .actions()
            .iter()
            .all(|a| a.status() == ActionStatus::Started)
    );
    conversation
        .recover_tools(
            &mut original_tools,
            ExecutionControl::new(&DefaultBudget::default()),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

struct FakeModel {
    calls: Arc<AtomicUsize>,
    content: Vec<AssistantContent>,
}
#[async_trait]
impl ModelAdapter for FakeModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelOutput::new(
            self.content.clone(),
            UsageReport::Unavailable,
        ))
    }
}
fn model(calls: Arc<AtomicUsize>, content: Vec<AssistantContent>) -> Model<'static> {
    Model::from_adapter("local", "recovery", FakeModel { calls, content }).unwrap()
}

#[tokio::test]
async fn nested_model_refusal_stays_typed_and_does_not_become_tool_feedback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let nested = model(calls.clone(), vec![AssistantContent::text("unused")]);
    let mut tools = ToolRegistry::default();
    tools
        .register(
            tool_with_context("work", "Nested model", move |_: Value, context| {
                let nested = nested.clone();
                Box::pin(async move {
                    context
                        .complete(&nested, ModelRequest::new("nested"))
                        .await?;
                    Ok("done")
                })
            })
            .unwrap(),
        )
        .unwrap();
    let mut conversation = interrupted(&tools);
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    assert!(matches!(
        conversation
            .recover_tools(&mut tools, ExecutionControl::new(&budget))
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().tool_calls, 1);
    assert_eq!(starts(&conversation), vec![1, 2]);
    assert!(
        conversation.responses()[0].actions()[0]
            .resolution()
            .is_none()
    );
}

#[tokio::test]
async fn live_incomplete_recovery_preserves_old_report_and_uses_a_fresh_scope() {
    let model_calls = Arc::new(AtomicUsize::new(0));
    let fake = model(
        model_calls.clone(),
        vec![AssistantContent::ToolCall(ToolCall::from_wire(
            "one",
            ToolFunction::new("work".into(), json!({})),
        ))],
    );
    let run = Harness::new(fake).run("input").tool(
        tool("work", "Stop after start", |_: Value| async {
            Err::<String, _>(ToolError::Stop(ExecutionStop::Stopped(
                "interrupted".into(),
            )))
        })
        .unwrap(),
    );
    let old = run.report();
    let mut progress = run.subscribe();
    let incomplete = run.await.unwrap_err();
    let old_snapshot = old.snapshot();
    let mut conversation = *incomplete.conversation;
    let effects = Arc::new(AtomicUsize::new(0));
    let mut tools = registry(effects.clone());
    assert!(matches!(
        conversation
            .recover_tools(
                &mut tools,
                ExecutionControl::new(&DefaultBudget::default()).with_report(&old)
            )
            .await,
        Err(ToolDispatchError::Recovery(RecoveryError::FinalizedReport))
    ));
    assert_eq!(old.snapshot(), old_snapshot);
    let fresh = ReportHandle::default();
    conversation
        .recover_tools(
            &mut tools,
            ExecutionControl::new(&DefaultBudget::default()).with_report(&fresh),
        )
        .await
        .unwrap();
    assert_eq!(old.snapshot(), old_snapshot);
    assert_eq!(fresh.snapshot().actions.len(), 1);
    assert!(fresh.snapshot().actions[0].started);
    assert!(fresh.snapshot().actions[0].outcome.is_some());
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    while progress.recv().await.is_some() {}
}

#[cfg(feature = "jsonl")]
#[tokio::test]
async fn jsonl_open_never_retries_and_writable_recovery_records_the_next_ordinal() {
    use frona_harness::Jsonl;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.jsonl");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = registry(calls.clone());
    let mut original = Conversation::new("input");
    original
        .attach_jsonl(Jsonl::create(&path).unwrap())
        .unwrap();
    let actions = record(&mut original, &["work"], &tools);
    original.start(&actions[0]).unwrap();
    drop(original);
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut writable = loaded.into_writable().unwrap();
    tokio::spawn(async move {
        writable
            .recover_tools(&mut tools, ExecutionControl::new(&DefaultBudget::default()))
            .await
            .unwrap();
    })
    .await
    .unwrap();
    let loaded = Jsonl::open(&path).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(starts(loaded.conversation()), vec![1, 2]);
    assert_eq!(loaded.conversation().pending_count(), 0);
}
