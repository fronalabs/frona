use std::{
    collections::VecDeque,
    future::IntoFuture,
    num::NonZeroUsize,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use frona_harness::{
    ActionStatus, BudgetLimits, Capabilities, Decision, DefaultBudget, ExecutionReport, Harness,
    Model, ModelAdapter, ModelOutput, ModelRequest, Progress, RefinementStage, RunOutcome,
    UsageReport,
    inference::{InferenceError, Usage, provider::StreamToken},
    tool,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Notify, mpsc};

fn capacity(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn output(content: AssistantContent) -> ModelOutput {
    ModelOutput::new(
        vec![content],
        UsageReport::Reported(Usage::new().total_tokens(3)),
    )
}
fn call(name: &str, value: u32) -> ModelOutput {
    output(AssistantContent::ToolCall(ToolCall::from_wire(
        "same-id",
        ToolFunction::new(name.try_into().unwrap(), json!({"value": value})),
    )))
}
#[derive(Deserialize, JsonSchema)]
struct Answer {
    value: u32,
}

struct Script<'a> {
    outputs: Mutex<VecDeque<ModelOutput>>,
    calls: &'a AtomicUsize,
    streams: &'a AtomicUsize,
    streaming: bool,
    retained_sender: Option<&'a Mutex<Option<mpsc::Sender<StreamToken>>>>,
}
#[async_trait]
impl ModelAdapter for Script<'_> {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            streaming: self.streaming,
            ..Capabilities::default()
        }
    }
    async fn generate(
        &self,
        _: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(tokens) = tokens {
            self.streams.fetch_add(1, Ordering::SeqCst);
            // More than the private token buffer: forwarding must run concurrently.
            for n in 0..100 {
                tokens.send(StreamToken::Text(n.to_string())).await.unwrap();
            }
            tokens
                .send(StreamToken::Reasoning("thinking".into()))
                .await
                .unwrap();
            if let Some(retained) = self.retained_sender {
                *retained.lock().unwrap() = Some(tokens);
            }
        }
        Ok(self.outputs.lock().unwrap().pop_front().unwrap())
    }
}
fn model<'a>(
    outputs: Vec<ModelOutput>,
    calls: &'a AtomicUsize,
    streams: &'a AtomicUsize,
    streaming: bool,
) -> Model<'a> {
    Model::from_adapter(
        "fake",
        "progress",
        Script {
            outputs: Mutex::new(outputs.into()),
            calls,
            streams,
            streaming,
            retained_sender: None,
        },
    )
    .unwrap()
}
fn normalize(report: &mut ExecutionReport) {
    for request in &mut report.requests {
        if let Some(input) = &mut request.input {
            input.conversation = uuid::Uuid::nil();
        }
        if let Some(action) = &mut request.parent_action {
            action.conversation = uuid::Uuid::nil();
        }
    }
    for action in &mut report.actions {
        action.position.conversation = uuid::Uuid::nil();
    }
    for feedback in &mut report.feedback {
        feedback.conversation = uuid::Uuid::nil();
        if let Some(action) = &mut feedback.action {
            action.conversation = uuid::Uuid::nil();
        }
    }
}

#[tokio::test]
async fn no_lagging_or_dropped_subscriber_changes_outcome_or_reliable_report() {
    let mut expected = None;
    for mode in 0..3 {
        let calls = AtomicUsize::new(0);
        let streams = AtomicUsize::new(0);
        let run = Harness::new(model(
            vec![call("echo", 7), output(AssistantContent::text("done"))],
            &calls,
            &streams,
            true,
        ))
        .run("input")
        .tool(
            tool(
                "echo",
                "Echo",
                |answer: Answer| async move { Ok(answer.value) },
            )
            .unwrap(),
        );
        let report = run.report();
        let mut receiver = match mode {
            0 => None,
            1 => Some(run.subscribe_with_capacity(capacity(2))),
            _ => {
                drop(run.subscribe());
                None
            }
        };
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "subscription must not start work"
        );
        assert_eq!(run.await.unwrap(), "done");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            streams.load(Ordering::SeqCst),
            if mode == 1 { 2 } else { 0 }
        );
        let mut snapshot = report.snapshot();
        normalize(&mut snapshot);
        assert_eq!(snapshot.outcome, Some(RunOutcome::Completed));
        if let Some(expected) = &expected {
            assert_eq!(&snapshot, expected);
        } else {
            expected = Some(snapshot);
        }
        if let Some(receiver) = &mut receiver {
            assert!(matches!(receiver.recv().await, Some(Progress::Missed(n)) if n > 0));
            let mut received = 0;
            while receiver.recv().await.is_some() {
                received += 1;
            }
            assert_eq!(received, 2);
        }
    }
}

#[tokio::test]
async fn nonstreaming_adapter_still_runs_with_observation_and_subscribers_are_independent() {
    let calls = AtomicUsize::new(0);
    let streams = AtomicUsize::new(0);
    let run = Harness::new(model(vec![call("submit", 1)], &calls, &streams, false))
        .run("input")
        .structured::<Answer>();
    let mut tiny = run.subscribe_with_capacity(capacity(1));
    let mut full = run.subscribe();
    assert_eq!(run.await.unwrap().value, 1);
    assert_eq!(streams.load(Ordering::SeqCst), 0);
    assert!(matches!(tiny.recv().await, Some(Progress::Missed(_))));
    assert!(matches!(
        tiny.recv().await,
        Some(Progress::Refinement {
            stage: RefinementStage::Accepted,
            ..
        })
    ));
    assert_eq!(tiny.recv().await, None);
    let mut stages = Vec::new();
    while let Some(progress) = full.recv().await {
        assert!(!matches!(progress, Progress::Missed(_)));
        if let Progress::Refinement { stage, .. } = progress {
            stages.push(stage);
        }
    }
    assert_eq!(stages, [RefinementStage::Ready, RefinementStage::Accepted]);
}

#[tokio::test]
async fn retained_provider_sender_cannot_keep_call_or_subscription_alive() {
    let calls = AtomicUsize::new(0);
    let streams = AtomicUsize::new(0);
    let retained = Mutex::new(None);
    let model = Model::from_adapter(
        "fake",
        "retained",
        Script {
            outputs: Mutex::new(vec![output(AssistantContent::text("done"))].into()),
            calls: &calls,
            streams: &streams,
            streaming: true,
            retained_sender: Some(&retained),
        },
    )
    .unwrap();
    let run = Harness::new(model).run("input");
    let report = run.report();
    let mut receiver = run.subscribe_with_capacity(capacity(2));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), run.into_future())
            .await
            .unwrap()
            .unwrap(),
        "done"
    );
    assert!(retained.lock().unwrap().as_ref().unwrap().is_closed());
    assert_eq!(receiver.recv().await, Some(Progress::Missed(99)));
    assert!(matches!(receiver.recv().await, Some(Progress::Text { text, .. }) if text == "99"));
    assert!(
        matches!(receiver.recv().await, Some(Progress::Reasoning { text, .. }) if text == "thinking")
    );
    assert_eq!(receiver.recv().await, None);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));
}

#[tokio::test]
async fn manual_terminal_and_dropped_run_close_even_with_report_and_conversation_alive() {
    let calls = AtomicUsize::new(0);
    let streams = AtomicUsize::new(0);
    let harness = Harness::new(model(
        vec![output(AssistantContent::text("done"))],
        &calls,
        &streams,
        false,
    ));
    let mut session = harness.run("input").manual();
    let report = session.report();
    let mut receiver = session.subscribe();
    session.call_model().await.unwrap();
    assert_eq!(session.finish().unwrap(), "done");
    assert_eq!(receiver.recv().await, None);
    assert_eq!(session.subscribe().recv().await, None);
    assert_eq!(session.conversation().responses().len(), 1);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));

    let run = harness.run("abandoned");
    let report = run.report();
    let mut receiver = run.subscribe();
    drop(run);
    assert_eq!(receiver.recv().await, None);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Abandoned));

    let run = harness
        .run("refused")
        .budget(DefaultBudget::new(BudgetLimits {
            model_attempts: Some(0),
            ..BudgetLimits::default()
        }));
    let report = run.report();
    let mut receiver = run.subscribe();
    let incomplete = run.await.unwrap_err();
    assert_eq!(receiver.recv().await, None);
    assert_eq!(*incomplete.report, report.snapshot());
    assert_eq!(incomplete.conversation.responses().len(), 0);
}

#[tokio::test]
async fn tool_activity_is_visible_while_tool_runs_and_receiver_drop_does_not_drive_it() {
    let calls = AtomicUsize::new(0);
    let streams = AtomicUsize::new(0);
    let release = Notify::new();
    let entered = Notify::new();
    let run = Harness::new(model(
        vec![call("echo", 7), output(AssistantContent::text("done"))],
        &calls,
        &streams,
        false,
    ))
    .run("input")
    .tool(
        tool("echo", "Echo", |answer: Answer| {
            let entered = &entered;
            let release = &release;
            async move {
                entered.notify_one();
                release.notified().await;
                Ok(answer.value)
            }
        })
        .unwrap(),
    );
    let mut receiver = run.subscribe_with_capacity(capacity(2));
    let report = run.report();
    let (result, ()) = tokio::join!(async { run.await }, async {
        entered.notified().await;
        assert!(matches!(
            receiver.recv().await,
            Some(Progress::Action {
                status: ActionStatus::Proposed,
                ..
            })
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(Progress::Action {
                status: ActionStatus::Started,
                ..
            })
        ));
        assert!(report.snapshot().actions[0].outcome.is_none());
        drop(receiver);
        release.notify_one();
    });
    assert_eq!(result.unwrap(), "done");
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));
}

#[tokio::test]
async fn borrowed_local_inspector_reports_refinement_without_promoting_retained_work() {
    let calls = AtomicUsize::new(0);
    let streams = AtomicUsize::new(0);
    let run = Harness::new(model(
        vec![call("submit", 1), call("submit", 2)],
        &calls,
        &streams,
        false,
    ))
    .run("input")
    .structured::<Answer>();
    let mut receiver = run.subscribe();
    let local = std::rc::Rc::new(std::cell::Cell::new(0));
    let incomplete = run
        .inspect(async |answer| {
            local.set(answer.value);
            tokio::task::yield_now().await;
            Ok::<_, ()>(if answer.value == 1 {
                Decision::revise("more").retain(answer.value)
            } else {
                Decision::stop("enough")
            })
        })
        .await
        .unwrap_err();
    assert_eq!(incomplete.retained, Some(1));
    assert_eq!(local.get(), 2);
    let mut stages = Vec::new();
    while let Some(progress) = receiver.recv().await {
        if let Progress::Refinement { stage, .. } = progress {
            stages.push(stage);
        }
    }
    assert_eq!(
        stages,
        [
            RefinementStage::Ready,
            RefinementStage::Inspecting,
            RefinementStage::Retained,
            RefinementStage::Revised,
            RefinementStage::Ready,
            RefinementStage::Inspecting
        ]
    );
}

struct PausedStream<'a> {
    produced: &'a Notify,
    release: &'a Notify,
}
#[async_trait]
impl ModelAdapter for PausedStream<'_> {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            ..Capabilities::default()
        }
    }
    async fn generate(
        &self,
        _: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        let tokens = tokens.expect("subscribed streaming call");
        for n in 0..8 {
            tokens.send(StreamToken::Text(n.to_string())).await.unwrap();
        }
        self.produced.notify_one();
        self.release.notified().await;
        for n in 8..108 {
            tokens.send(StreamToken::Text(n.to_string())).await.unwrap();
        }
        Ok(output(AssistantContent::text("done")))
    }
}

#[tokio::test]
async fn controlled_stream_burst_reports_exact_loss_before_completion_and_survives_disconnect() {
    let produced = Notify::new();
    let release = Notify::new();
    let model = Model::from_adapter(
        "fake",
        "paused",
        PausedStream {
            produced: &produced,
            release: &release,
        },
    )
    .unwrap();
    let run = Harness::new(model).run("input");
    let mut receiver = run.subscribe_with_capacity(capacity(2));
    let report = run.report();
    let (outcome, ()) = tokio::join!(async { run.await }, async {
        produced.notified().await;
        assert_eq!(receiver.recv().await, Some(Progress::Missed(6)));
        for n in [6, 7] {
            assert!(
                matches!(receiver.recv().await, Some(Progress::Text { text, .. }) if text == n.to_string())
            );
        }
        assert_eq!(report.snapshot().outcome, None);
        drop(receiver);
        release.notify_one();
    });
    assert_eq!(outcome.unwrap(), "done");
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));
}

#[tokio::test]
async fn cancelling_or_dropping_inflight_stream_closes_subscription_without_a_final_event() {
    for cancel in [false, true] {
        let produced = Notify::new();
        let release = Notify::new();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let model = Model::from_adapter(
            "fake",
            "paused",
            PausedStream {
                produced: &produced,
                release: &release,
            },
        )
        .unwrap();
        let run = Harness::new(model)
            .run("input")
            .cancellation(cancellation.clone());
        let report = run.report();
        let mut receiver = run.subscribe_with_capacity(capacity(2));
        let mut future = Box::pin(run.into_future());
        tokio::select! {
            result = &mut future => panic!("provider should be paused: {result:?}"),
            _ = produced.notified() => {},
        }
        if cancel {
            cancellation.cancel();
            let incomplete = future.await.unwrap_err();
            assert_eq!(*incomplete.report, report.snapshot());
        } else {
            drop(future);
            assert_eq!(report.snapshot().outcome, Some(RunOutcome::Abandoned));
        }
        while receiver.recv().await.is_some() {}
    }
}
