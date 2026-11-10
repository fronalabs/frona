use std::{collections::VecDeque, future::IntoFuture, sync::Mutex};

use async_trait::async_trait;
use frona_harness::{
    ActionOutcome, AttemptId, Budget, BudgetLimits, BudgetOperation, BudgetPermit, BudgetRefusal,
    CallOutcome, Capabilities, Decision, DefaultBudget, FailureKind, Harness, Model, ModelAdapter,
    ModelOutput, ModelRequest, RunOutcome, UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
    tool, tool_with_context,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

struct Script(Mutex<VecDeque<ModelOutput>>);
#[async_trait]
impl ModelAdapter for Script {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            ..Capabilities::default()
        }
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider call"))
    }
}
fn model(outputs: Vec<ModelOutput>) -> Model<'static> {
    Model::from_adapter("fake", "report", Script(Mutex::new(outputs.into()))).unwrap()
}
fn output(content: Vec<AssistantContent>) -> ModelOutput {
    ModelOutput::new(content, UsageReport::Reported(Usage::new().total_tokens(3)))
}
fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        id,
        ToolFunction::new(name.try_into().unwrap(), arguments),
    ))
}
fn submitted(value: u32) -> ModelOutput {
    output(vec![call("submit", "submit", json!({"value":value}))])
}
#[derive(Deserialize, JsonSchema)]
struct Answer {
    value: u32,
}

#[tokio::test]
async fn inspector_reads_independent_report_and_feedback_correlates_to_actual_request() {
    let run = Harness::new(model(vec![submitted(1), submitted(2)]))
        .run("refine")
        .structured::<Answer>();
    let report = run.report();
    assert_eq!(report.snapshot().logical_calls(), 0);
    let mut inspected = Vec::new();
    let result = run
        .inspect(async |answer| {
            let snapshot = report.snapshot();
            inspected.push(snapshot.logical_calls());
            assert_eq!(snapshot.provider_attempts(), answer.value as usize);
            assert_eq!(snapshot.outcome, None);
            if answer.value == 1 {
                assert!(snapshot.feedback.is_empty());
                Ok::<_, ()>(Decision::revise("try again"))
            } else {
                let feedback = &snapshot.feedback[0];
                assert_eq!(feedback.action.unwrap().response, 0);
                assert_eq!(feedback.message, Some(2));
                assert_eq!(feedback.issued_in, [AttemptId(1)]);
                let input = snapshot.requests[1].input.as_ref().unwrap();
                assert_eq!(input.conversation, feedback.conversation);
                assert_eq!(input.messages, [0, 1, 2]);
                Ok(Decision::accept(answer.value))
            }
        })
        .await
        .unwrap();
    assert_eq!(result, 2);
    assert_eq!(inspected, [1, 2]);
    let final_report = report.snapshot();
    assert_eq!(final_report.outcome, Some(RunOutcome::Completed));
    // Acceptance acknowledgement is queued, but no third request was issued.
    assert!(final_report.feedback[1].issued_in.is_empty());
    assert_eq!(
        final_report.attempts[0].usage,
        Some(UsageReport::Reported(Usage::new().total_tokens(3)))
    );
}

#[tokio::test]
async fn refusal_leaves_queued_feedback_undelivered_and_final_snapshots_agree() {
    let run = Harness::new(model(vec![submitted(1)]))
        .run("refine")
        .budget(DefaultBudget::new(BudgetLimits {
            model_attempts: Some(1),
            ..BudgetLimits::default()
        }))
        .structured::<Answer>();
    let report = run.report();
    let incomplete = run
        .inspect(async |_| Ok::<_, ()>(Decision::<()>::revise("more evidence")))
        .await
        .unwrap_err();
    let snapshot = report.snapshot();
    assert_eq!(*incomplete.report, snapshot);
    assert_eq!(snapshot.logical_calls(), 2);
    assert_eq!(snapshot.provider_attempts(), 1);
    assert!(snapshot.feedback[0].issued_in.is_empty());
    assert_eq!(snapshot.feedback[0].message, Some(2));
    assert_eq!(snapshot.requests[1].outcome, Some(CallOutcome::Failed));
    assert!(matches!(
        snapshot.outcome,
        Some(RunOutcome::Incomplete {
            kind: FailureKind::Budget,
            ..
        })
    ));
}

#[tokio::test]
async fn cancellation_inside_admission_does_not_claim_feedback_dispatch() {
    struct CancelSecond {
        token: CancellationToken,
        admissions: Mutex<usize>,
    }
    impl Budget for CancelSecond {
        fn admit(&self, _: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
            let mut count = self.admissions.lock().unwrap();
            *count += 1;
            if *count == 2 {
                self.token.cancel();
            }
            Ok(BudgetPermit::new(|_| {}))
        }
    }
    let token = CancellationToken::new();
    let run = Harness::new(model(vec![submitted(1)]))
        .run("refine")
        .cancellation(token.clone())
        .budget(CancelSecond {
            token,
            admissions: Mutex::new(0),
        })
        .structured::<Answer>();
    let report = run.report();
    let incomplete = run
        .inspect(async |_| Ok::<_, ()>(Decision::<()>::revise("more")))
        .await
        .unwrap_err();
    assert_eq!(*incomplete.report, report.snapshot());
    assert_eq!(incomplete.report.logical_calls(), 2);
    assert_eq!(incomplete.report.provider_attempts(), 1);
    assert!(incomplete.report.feedback[0].issued_in.is_empty());
    assert_eq!(
        incomplete.report.requests[1].outcome,
        Some(CallOutcome::Cancelled)
    );
}

#[tokio::test]
async fn partial_batch_has_action_evidence_before_replay_message_and_nested_calls_do_not_deliver_it()
 {
    let nested = model(vec![output(vec![AssistantContent::text("nested")])]);
    let run = Harness::new(model(vec![
        output(vec![
            call("duplicate", "first", json!({})),
            call("duplicate", "nested", json!({})),
        ]),
        output(vec![AssistantContent::text("done")]),
    ]))
    .run("tools")
    .tool(tool("first", "First", |_: Value| async { Ok("first") }).unwrap())
    .tool(
        tool_with_context("nested", "Nested", move |_: Value, context| {
            let nested = nested.clone();
            Box::pin(async move {
                context
                    .complete(&nested, ModelRequest::new("nested only"))
                    .await?;
                Ok("nested done")
            })
        })
        .unwrap(),
    );
    let report = run.report();
    let mut manual = run.manual();
    let mut actions = manual.call_model().await.unwrap().actions.into_iter();
    manual.reject(actions.next().unwrap(), "rejected").unwrap();
    let partial = report.snapshot();
    assert_eq!(partial.feedback[0].message, None);
    assert_eq!(partial.feedback[0].action.unwrap().action, 0);
    manual.execute(actions.next().unwrap()).await.unwrap();
    let nested = report.snapshot();
    assert_eq!(nested.logical_calls(), 2);
    assert_eq!(nested.requests[1].input, None);
    assert_eq!(
        nested.requests[1].parent_action,
        Some(nested.actions[1].position)
    );
    assert_eq!(nested.actions[0].outcome, Some(ActionOutcome::Rejected));
    assert!(!nested.actions[0].started);
    assert!(nested.actions[1].started);
    assert_eq!(nested.actions[1].outcome, Some(ActionOutcome::Resolved));
    assert!(
        nested
            .feedback
            .iter()
            .all(|feedback| feedback.message == Some(2) && feedback.issued_in.is_empty())
    );
    manual.call_model().await.unwrap();
    assert_eq!(manual.finish().unwrap(), "done");
    let final_report = report.snapshot();
    assert!(
        final_report
            .feedback
            .iter()
            .all(|feedback| feedback.issued_in == [AttemptId(2)])
    );
    assert_eq!(final_report.outcome, Some(RunOutcome::Completed));
}

#[tokio::test]
async fn reports_are_per_run_even_with_shared_budget() {
    let harness = Harness::new(model(vec![
        output(vec![AssistantContent::text("one")]),
        output(vec![AssistantContent::text("two")]),
    ]));
    let budget = DefaultBudget::default();
    for expected in ["one", "two"] {
        let run = harness.run("shared").budget(&budget);
        let report = run.report();
        assert_eq!(run.await.unwrap(), expected);
        assert_eq!(report.snapshot().logical_calls(), 1);
        assert_eq!(report.snapshot().provider_attempts(), 1);
        assert_eq!(
            report.snapshot().attempts[0].usage,
            Some(UsageReport::Reported(Usage::new().total_tokens(3)))
        );
    }
    assert_eq!(budget.snapshot().model_attempts, 2);
    assert_eq!(budget.snapshot().tokens, 6);
}

struct Waiting<'a>(&'a Notify);
#[async_trait]
impl ModelAdapter for Waiting<'_> {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn dropped_run_and_inflight_provider_have_abandonment_evidence() {
    let started = Notify::new();
    let model = Model::from_adapter("fake", "waiting", Waiting(&started)).unwrap();
    let run = Harness::new(model).run("wait");
    let report = run.report();
    let mut future = run.into_future();
    tokio::select! {
        result = &mut future => panic!("unexpected result: {result:?}"),
        _ = started.notified() => {}
    }
    assert_eq!(report.snapshot().attempts[0].outcome, None);
    drop(future);
    let final_report = report.snapshot();
    assert_eq!(final_report.outcome, Some(RunOutcome::Abandoned));
    assert_eq!(
        final_report.requests[0].outcome,
        Some(CallOutcome::Abandoned)
    );
    assert_eq!(
        final_report.attempts[0].outcome,
        Some(CallOutcome::Abandoned)
    );
    assert_eq!(
        final_report.attempts[0].usage,
        Some(UsageReport::Unavailable)
    );
}

#[tokio::test]
async fn dropped_tool_future_records_abandonment_without_inventing_feedback() {
    let started = Notify::new();
    let waiting = tool("wait", "Wait", |_: Value| async {
        started.notify_one();
        std::future::pending::<Result<String, frona_harness::ToolError>>().await
    })
    .unwrap();
    let run = Harness::new(model(vec![output(vec![call("wait", "wait", json!({}))])]))
        .run("tool")
        .tool(waiting);
    let report = run.report();
    let mut future = run.into_future();
    tokio::select! {
        result = &mut future => panic!("unexpected result: {result:?}"),
        _ = started.notified() => {}
    }
    drop(future);
    let snapshot = report.snapshot();
    assert_eq!(snapshot.outcome, Some(RunOutcome::Abandoned));
    assert_eq!(snapshot.actions[0].outcome, Some(ActionOutcome::Abandoned));
    assert!(snapshot.feedback.is_empty());
}
