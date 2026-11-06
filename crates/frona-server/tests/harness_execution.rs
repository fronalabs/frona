#[path = "../../frona-harness/tests/common/execution_support.rs"]
mod support;
use support::*;

use frona::{
    chat::broadcast::EventSender,
    core::error::AppError,
    inference::streaming::{ModelGroupStreaming, StreamResult},
};
use frona_harness::{
    BudgetRefusal, ExecutionControl, ExecutionStop,
    inference::{
        InferenceError,
        observation::{AttemptOutcome, Observations},
    },
};
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn streaming_refusal_and_partial_output_never_replay() {
    for partial in [false, true] {
        let fake = Arc::new(Fake::new([if partial {
            Step::Partial(rate())
        } else {
            Step::Fail(rate())
        }]));
        let group = group(fake.clone());
        let budget = limited(if partial { 5 } else { 1 });
        let observer = Observer::default();
        let error = group
            .stream_inference_controlled(
                request(Observations::new(&observer)),
                &EventSender::noop(),
                ExecutionControl::new(&budget),
                &mut String::new(),
            )
            .await
            .err()
            .unwrap();
        if !partial {
            assert!(matches!(error, AppError::Inference(error) if error.is_terminal()));
        }
        assert_eq!(*fake.calls.lock().unwrap(), ["primary"]);
        assert_eq!(observer.0.lock().unwrap().len(), 1);
        assert_eq!(budget.snapshot().in_flight, 0);
    }
}

#[tokio::test]
async fn streaming_cancellation_drains_partial_text_and_never_succeeds() {
    let (started, started_rx) = oneshot::channel();
    let (_release, release_rx) = oneshot::channel();
    let fake = Arc::new(Fake::new([Step::Wait(started, release_rx, 7)]));
    let group = group(fake.clone());
    let token = CancellationToken::new();
    let budget = limited(5);
    let observer = Observer::default();
    let events = EventSender::noop();
    let mut text = String::new();
    let call = group.stream_inference_controlled(
        request(Observations::new(&observer)),
        &events,
        ExecutionControl::new(&budget).with_cancellation(&token),
        &mut text,
    );
    let cancel = async {
        started_rx.await.unwrap();
        token.cancel();
    };
    let (result, ()) = tokio::join!(call, cancel);
    assert!(matches!(result.unwrap(), StreamResult::Cancelled));
    assert_eq!(text, "partial");
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    assert_eq!(*observer.0.lock().unwrap(), [AttemptOutcome::Cancelled]);
    assert_eq!(budget.snapshot().in_flight, 0);
    // A pre-cancelled stream creates no attempt and still joins its forwarding task.
    let fake = Arc::new(Fake::new([]));
    let group = self::group(fake.clone());
    let result = group
        .stream_inference_controlled(
            request(Observations::default()),
            &events,
            ExecutionControl::new(&budget).with_cancellation(&token),
            &mut String::new(),
        )
        .await
        .unwrap();
    assert!(matches!(result, StreamResult::Cancelled));
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn app_error_preserves_typed_execution_stops() {
    let error: InferenceError =
        ExecutionStop::BudgetExhausted(BudgetRefusal::Denied("application policy".into())).into();
    let AppError::Inference(error) = AppError::from(error) else {
        panic!("inference error was flattened")
    };
    assert!(
        matches!(error.into_execution_stop(), Ok(ExecutionStop::BudgetExhausted(BudgetRefusal::Denied(reason))) if reason == "application policy")
    );
}
