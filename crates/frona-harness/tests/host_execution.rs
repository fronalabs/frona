use frona_harness::{ExecutionStop, FailureKind, ReportHandle, RunOutcome};
use std::{
    future::{Future, pending, poll_fn},
    task::Poll,
};

#[test]
fn dropping_unpolled_host_scope_records_abandonment() {
    let report = ReportHandle::default();
    let run = report.record_run(async { ((), RunOutcome::Completed) });
    drop(run);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Abandoned));
}

#[tokio::test]
async fn unfinished_final_write_is_abandoned_and_failed_final_write_overrides_candidate_success() {
    let report = ReportHandle::default();
    let mut run = Box::pin(report.record_run(async {
        // A model answer exists; required host persistence is still unfinished.
        pending::<()>().await;
        ("candidate", RunOutcome::Completed)
    }));
    poll_fn(|cx| {
        assert!(run.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(report.snapshot().outcome, None);
    drop(run);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Abandoned));

    let report = ReportHandle::default();
    let result = report
        .record_run(async {
            let candidate = "candidate";
            let failure =
                ExecutionStop::Storage(Box::new(std::io::Error::other("final write failed")));
            (candidate, RunOutcome::from(&failure))
        })
        .await;
    assert_eq!(result, "candidate");
    assert!(matches!(
        report.snapshot().outcome,
        Some(RunOutcome::Incomplete {
            kind: FailureKind::Storage,
            ..
        })
    ));
}

#[tokio::test]
async fn outer_scope_preserves_first_terminal_outcome_and_distinct_stop_kinds() {
    for (stop, kind) in [
        (
            ExecutionStop::Cancelled("cancelled".into()),
            FailureKind::Cancelled,
        ),
        (
            ExecutionStop::Stopped("server shutdown".into()),
            FailureKind::ApplicationStop,
        ),
    ] {
        let report = ReportHandle::default();
        report
            .record_run(async { ((), RunOutcome::from(&stop)) })
            .await;
        let first = report.snapshot();
        assert!(
            matches!(first.outcome, Some(RunOutcome::Incomplete { kind: actual, .. }) if actual == kind)
        );
        report
            .record_run(async { ((), RunOutcome::Completed) })
            .await;
        assert_eq!(report.snapshot(), first);
    }
}

use frona_harness::{
    ActionResolution, Capabilities, Conversation, DefaultBudget, ExecutionControl, Model,
    ModelAdapter, ModelOutput, ModelRequest, ToolError, ToolOutput, ToolRegistry, UsageReport,
    inference::{InferenceError, observation::Observations, provider::StreamToken},
    tool,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Args {}

fn proposal() -> ModelOutput {
    ModelOutput::new(
        vec![AssistantContent::ToolCall(ToolCall::from_wire(
            "call",
            ToolFunction::new("wait".into(), serde_json::json!({})),
        ))],
        UsageReport::Unavailable,
    )
}

#[tokio::test]
async fn host_completion_rejects_unresolved_or_stopped_actions_but_allows_terminal_feedback() {
    for mode in ["unresolved", "stopped", "resolved", "failed", "rejected"] {
        let budget = DefaultBudget::default();
        let report = ReportHandle::default();
        let execution = ExecutionControl::new(&budget).with_report(&report);
        let mut conversation = Conversation::from_execution(execution);
        let action = conversation
            .record_response(proposal(), vec![])
            .unwrap()
            .actions
            .remove(0);
        match mode {
            "unresolved" => {}
            "stopped" => {
                conversation.start(&action).unwrap();
                conversation.stop().unwrap();
            }
            _ => {
                let feedback = ToolOutput::text("feedback");
                let resolution = match mode {
                    "resolved" => ActionResolution::Result(feedback),
                    "failed" => ActionResolution::Failed(feedback),
                    _ => ActionResolution::Rejected(feedback),
                };
                conversation.resolve(&action, resolution).unwrap();
                conversation.finish().unwrap();
            }
        }
        report
            .record_run(async { ((), RunOutcome::Completed) })
            .await;
        if matches!(mode, "unresolved" | "stopped") {
            assert!(
                matches!(
                    report.snapshot().outcome,
                    Some(RunOutcome::Incomplete {
                        kind: FailureKind::Protocol,
                        ..
                    })
                ),
                "{mode}"
            );
        } else {
            assert_eq!(
                report.snapshot().outcome,
                Some(RunOutcome::Completed),
                "{mode}"
            );
        }
    }
}

#[tokio::test]
async fn host_completion_rejects_an_abandoned_tool_invocation() {
    let budget = DefaultBudget::default();
    let report = ReportHandle::default();
    let execution = ExecutionControl::new(&budget).with_report(&report);
    let mut conversation = Conversation::from_execution(execution);
    let mut tools = ToolRegistry::default();
    tools
        .register(
            tool("wait", "wait", |_: Args| {
                pending::<Result<String, ToolError>>()
            })
            .unwrap(),
        )
        .unwrap();
    let action = conversation
        .record_response(proposal(), tools.definitions().cloned().collect())
        .unwrap()
        .actions
        .remove(0);
    let mut invocation = Box::pin(conversation.execute_tool(&action, &mut tools, execution));
    poll_fn(|cx| {
        assert!(invocation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(invocation);
    assert_eq!(
        report.snapshot().actions[0].outcome,
        Some(frona_harness::ActionOutcome::Abandoned)
    );
    report
        .record_run(async { ((), RunOutcome::Completed) })
        .await;
    assert!(matches!(
        report.snapshot().outcome,
        Some(RunOutcome::Incomplete {
            kind: FailureKind::Protocol,
            ..
        })
    ));
}

struct Provider {
    pending: bool,
}
#[async_trait::async_trait]
impl ModelAdapter for Provider {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: Option<tokio::sync::mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        if self.pending {
            pending().await
        } else {
            Err(InferenceError::InferenceFailed(
                "recoverable provider failure".into(),
            ))
        }
    }
}

#[tokio::test]
async fn host_completion_rejects_inflight_requests_and_attempts_but_allows_finished_failure() {
    for attempt in [false, true] {
        let model = Model::from_adapter("fake", "pending", Provider { pending: true }).unwrap();
        let budget = DefaultBudget::default();
        let report = ReportHandle::default();
        let execution = ExecutionControl::new(&budget).with_report(&report);
        let mut invocation: std::pin::Pin<Box<dyn Future<Output = ()>>> = if attempt {
            Box::pin(async {
                let _ = model
                    .invoke_controlled(
                        ModelRequest::new("input"),
                        None,
                        Observations::default(),
                        execution,
                    )
                    .await;
            })
        } else {
            Box::pin(async {
                let _ = execution
                    .model_call(model.config(), |_| pending::<Result<(), InferenceError>>())
                    .await;
            })
        };
        report
            .record_run(async {
                poll_fn(|cx| {
                    assert!(invocation.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                ((), RunOutcome::Completed)
            })
            .await;
        assert_eq!(report.snapshot().attempts.len(), usize::from(attempt));
        assert!(matches!(
            report.snapshot().outcome,
            Some(RunOutcome::Incomplete {
                kind: FailureKind::Protocol,
                ..
            })
        ));
        drop(invocation);
    }
    let model = Model::from_adapter("fake", "failed", Provider { pending: false }).unwrap();
    let budget = DefaultBudget::default();
    let report = ReportHandle::default();
    report
        .record_run(async {
            assert!(
                model
                    .invoke_controlled(
                        ModelRequest::new("input"),
                        None,
                        Observations::default(),
                        ExecutionControl::new(&budget).with_report(&report)
                    )
                    .await
                    .is_err()
            );
            ((), RunOutcome::Completed)
        })
        .await;
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));
    assert_eq!(
        report.snapshot().attempts[0].outcome,
        Some(frona_harness::CallOutcome::Failed)
    );
}
