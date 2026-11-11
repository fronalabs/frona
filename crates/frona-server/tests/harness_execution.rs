#[path = "../../frona-harness/tests/helpers/rig.rs"]
mod rig;
#[path = "../../frona-harness/tests/helpers/execution.rs"]
mod support;
use support::*;

use frona::{
    chat::broadcast::EventSender,
    core::error::AppError,
    inference::streaming::{ModelGroupStreaming, StreamResult},
};
use frona_harness::{
    BudgetRefusal, CallOutcome, ExecutionControl, ExecutionStop, ReportHandle,
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
        let report = ReportHandle::default();
        let error = group
            .stream_inference(
                request(Observations::new(&observer)),
                &EventSender::noop(),
                ExecutionControl::new(&budget).with_report(&report),
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
        let snapshot = report.snapshot();
        assert_eq!(snapshot.logical_calls(), 1);
        assert_eq!(snapshot.provider_attempts(), 1);
        assert_eq!(snapshot.requests[0].outcome, Some(CallOutcome::Failed));
        assert_eq!(snapshot.attempts[0].request, snapshot.requests[0].id);
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
    let report = ReportHandle::default();
    let call = group.stream_inference(
        request(Observations::new(&observer)),
        &events,
        ExecutionControl::new(&budget)
            .with_cancellation(&token)
            .with_report(&report),
        &mut text,
    );
    let cancel = async {
        started_rx.await.unwrap();
        token.cancel();
    };
    let (result, ()) = tokio::join!(call, cancel);
    assert!(matches!(result.unwrap(), StreamResult::Cancelled));
    assert_eq!(text, NATIVE_PARTIAL);
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    assert_eq!(*observer.0.lock().unwrap(), [AttemptOutcome::Cancelled]);
    assert_eq!(budget.snapshot().in_flight, 0);
    let snapshot = report.snapshot();
    assert_eq!(snapshot.logical_calls(), 1);
    assert_eq!(snapshot.provider_attempts(), 1);
    assert_eq!(snapshot.requests[0].outcome, Some(CallOutcome::Cancelled));
    assert_eq!(snapshot.attempts[0].outcome, Some(CallOutcome::Cancelled));
    // A pre-cancelled stream creates no attempt and still joins its forwarding task.
    let fake = Arc::new(Fake::new([]));
    let group = self::group(fake.clone());
    let result = group
        .stream_inference(
            request(Observations::default()),
            &events,
            ExecutionControl::new(&budget)
                .with_cancellation(&token)
                .with_report(&report),
            &mut String::new(),
        )
        .await
        .unwrap();
    assert!(matches!(result, StreamResult::Cancelled));
    assert!(fake.calls.lock().unwrap().is_empty());
    let snapshot = report.snapshot();
    assert_eq!(snapshot.logical_calls(), 2);
    assert_eq!(snapshot.provider_attempts(), 1);
    assert_eq!(snapshot.requests[1].outcome, Some(CallOutcome::Cancelled));
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

#[tokio::test]
async fn streaming_report_groups_retry_attempts_and_records_abandonment() {
    let fake = Arc::new(Fake::new([Step::Fail(rate()), Step::Ok(3)]));
    let group = group(fake);
    let budget = limited(5);
    let report = ReportHandle::default();
    let events = EventSender::noop();
    let mut text = String::new();
    let result = group
        .stream_inference(
            request(Observations::default()),
            &events,
            ExecutionControl::new(&budget).with_report(&report),
            &mut text,
        )
        .await
        .unwrap();
    assert!(matches!(result, StreamResult::Contents(_)));
    let snapshot = report.snapshot();
    assert_eq!(snapshot.logical_calls(), 1);
    assert_eq!(snapshot.provider_attempts(), 2);
    assert_eq!(snapshot.requests[0].outcome, Some(CallOutcome::Succeeded));
    assert!(
        snapshot
            .attempts
            .iter()
            .all(|attempt| attempt.request == snapshot.requests[0].id)
    );
    assert_eq!(snapshot.attempts[0].outcome, Some(CallOutcome::Failed));
    assert_eq!(snapshot.attempts[1].outcome, Some(CallOutcome::Succeeded));

    let (started, started_rx) = oneshot::channel();
    let (_release, release_rx) = oneshot::channel();
    let group = self::group(Arc::new(Fake::new([Step::Wait(started, release_rx, 7)])));
    let mut call = Box::pin(group.stream_inference(
        request(Observations::default()),
        &events,
        ExecutionControl::new(&budget).with_report(&report),
        &mut text,
    ));
    tokio::select! {
        _ = started_rx => {},
        _ = &mut call => panic!("provider should remain in flight"),
    }
    drop(call);
    let snapshot = report.snapshot();
    assert_eq!(snapshot.logical_calls(), 2);
    assert_eq!(snapshot.provider_attempts(), 3);
    assert_eq!(snapshot.requests[1].outcome, Some(CallOutcome::Abandoned));
    assert_eq!(snapshot.attempts[2].outcome, Some(CallOutcome::Abandoned));
    assert_eq!(snapshot.attempts[2].request, snapshot.requests[1].id);
    assert_eq!(budget.snapshot().in_flight, 0);
}
