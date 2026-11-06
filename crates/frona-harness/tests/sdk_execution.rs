#[path = "common/execution_support.rs"]
mod support;
use support::*;

use async_trait::async_trait;
use frona_harness::{
    Budget, BudgetCompletion, BudgetLimits, BudgetOperation, BudgetPermit, BudgetRefusal,
    DefaultBudget, ExecutionControl, ExecutionStop, Model, ModelError, ModelRequest, UsageReport,
    inference::{
        InferenceError,
        observation::{
            AttemptObservation, AttemptOutcome, InferenceObserver, ObservationError, Observations,
        },
        provider::StreamToken,
    },
};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn refusal_between_retry_attempts_or_before_fallback_stops_all_providers() {
    for structured in [false, true] {
        for retryable in [false, true] {
            let fake = Arc::new(Fake::new([Step::Fail(if retryable {
                rate()
            } else {
                InferenceError::ConfigError("ordinary provider failure".into())
            })]));
            let group = group(fake.clone());
            let budget = limited(1);
            let observer = Observer::default();
            let error = run(
                &group,
                structured,
                ExecutionControl::new(&budget),
                &observer,
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error.into_execution_stop(),
                Ok(ExecutionStop::BudgetExhausted(
                    BudgetRefusal::LimitReached { .. }
                ))
            ));
            assert_eq!(*fake.calls.lock().unwrap(), ["primary"]);
            assert_eq!(observer.0.lock().unwrap().len(), 1);
            assert_eq!(budget.snapshot().model_attempts, 1);
            assert_eq!(budget.snapshot().in_flight, 0);
        }
    }
}

#[tokio::test]
async fn ordinary_retry_and_fallback_keep_accounting_and_rich_structured_usage() {
    for structured in [false, true] {
        for retryable in [false, true] {
            let fake = Arc::new(Fake::new([
                Step::Fail(if retryable {
                    rate()
                } else {
                    InferenceError::ConfigError("ordinary".into())
                }),
                Step::Ok(7),
            ]));
            let group = group(fake.clone());
            let budget = limited(3);
            let observer = Observer::default();
            run(
                &group,
                structured,
                ExecutionControl::new(&budget),
                &observer,
            )
            .await
            .unwrap();
            assert_eq!(
                *fake.calls.lock().unwrap(),
                ["primary", if retryable { "primary" } else { "fallback" }]
            );
            assert_eq!(budget.snapshot().model_attempts, 2);
            assert_eq!(budget.snapshot().tokens, 7);
            assert_eq!(observer.0.lock().unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn nested_typed_stops_escape_retry_and_fallback_wrappers() {
    for stop in [
        ExecutionStop::BudgetExhausted(BudgetRefusal::Denied("nested".into())),
        ExecutionStop::Storage("disk failed".into()),
        ExecutionStop::Cancelled("nested".into()),
    ] {
        let error = InferenceError::AllFallbacksFailed(vec![InferenceError::ModelFailed {
            provider: "nested".into(),
            model: "model".into(),
            retry_count: 3,
            source: Box::new(InferenceError::Stop(stop)),
        }]);
        let fake = Arc::new(Fake::new([Step::Fail(error)]));
        let group = group(fake.clone());
        let budget = limited(5);
        let error = run(
            &group,
            false,
            ExecutionControl::new(&budget),
            &Observer::default(),
        )
        .await
        .unwrap_err();
        assert!(error.is_terminal());
        assert!(!error.is_retryable());
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn convenience_group_budget_is_not_reset_for_retries() {
    let fake = Arc::new(Fake::new((0..65).map(|_| Step::Fail(rate()))));
    let mut group = group(fake.clone());
    group.retry.max_retries = 100;
    let error = group
        .inference(request(Observations::default()))
        .await
        .unwrap_err();
    assert!(matches!(
        error.into_execution_stop(),
        Ok(ExecutionStop::BudgetExhausted(_))
    ));
    assert_eq!(fake.calls.lock().unwrap().len(), 64);
}

#[tokio::test]
async fn sdk_refusal_and_cancellation_before_dispatch_emit_no_attempt() {
    let fake = Arc::new(Fake::new([]));
    let model = Model::from_adapter("fake", "model", FakeAdapter(fake.clone())).unwrap();
    let budget = limited(0);
    let observer = Observer::default();
    let result = model
        .invoke_controlled(
            ModelRequest::new("hi"),
            None,
            Observations::new(&observer),
            ExecutionControl::new(&budget),
        )
        .await;
    assert!(matches!(
        result,
        Err(ModelError::Provider(InferenceError::Stop(
            ExecutionStop::BudgetExhausted(_)
        )))
    ));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let budget = limited(1);
    let error = model
        .invoke_controlled(
            ModelRequest::new("hi"),
            None,
            Observations::new(&observer),
            ExecutionControl::new(&budget).with_cancellation(&cancellation),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Provider(error) if error.is_cancelled()));
    assert_eq!(budget.snapshot().model_attempts, 0);
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(observer.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_during_admission_is_not_reported_as_an_issued_attempt() {
    struct Cancelling<'a> {
        token: &'a CancellationToken,
        completions: Mutex<Vec<BudgetCompletion>>,
    }
    impl Budget for Cancelling<'_> {
        fn admit(&self, _: BudgetOperation<'_>) -> Result<BudgetPermit<'_>, BudgetRefusal> {
            self.token.cancel();
            Ok(BudgetPermit::new(|completion| {
                self.completions.lock().unwrap().push(completion)
            }))
        }
    }
    let token = CancellationToken::new();
    let budget = Cancelling {
        token: &token,
        completions: Mutex::new(vec![]),
    };
    let fake = Arc::new(Fake::new([]));
    let model = Model::from_adapter("fake", "model", FakeAdapter(fake.clone())).unwrap();
    let observer = Observer::default();
    let error = model
        .invoke_controlled(
            ModelRequest::new("hi"),
            None,
            Observations::new(&observer),
            ExecutionControl::new(&budget).with_cancellation(&token),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Provider(error) if error.is_cancelled()));
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(observer.0.lock().unwrap().is_empty());
    assert_eq!(budget.completions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn admitted_sdk_call_finishes_after_shared_budget_exhaustion() {
    let (started, started_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let fake = Arc::new(Fake::new([Step::Wait(started, release_rx, 7)]));
    let model = Model::from_adapter("fake", "model", FakeAdapter(fake)).unwrap();
    let budget = DefaultBudget::new(BudgetLimits {
        tokens: Some(10),
        ..Default::default()
    });
    let observer = Observer::default();
    let call = model.invoke_controlled(
        ModelRequest::new("hi"),
        None,
        Observations::new(&observer),
        ExecutionControl::new(&budget),
    );
    let consume = async {
        started_rx.await.unwrap();
        budget
            .admit(BudgetOperation::Model {
                provider: "other",
                model: "other",
            })
            .unwrap()
            .complete(BudgetCompletion {
                outcome: frona_harness::BudgetOutcome::Succeeded,
                usage: UsageReport::Reported(usage(12)),
            });
        assert!(
            budget
                .admit(BudgetOperation::Tool { name: "blocked" })
                .is_err()
        );
        release.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(call, consume);
    result.unwrap();
    assert_eq!(budget.snapshot().tokens, 19);
    assert_eq!(budget.snapshot().in_flight, 0);
}

#[tokio::test]
async fn cancellation_drops_inflight_sdk_call_and_preserves_completed_usage_on_race() {
    let token = CancellationToken::new();
    let budget = limited(5);
    let observer = Observer::default();
    let (started, started_rx) = oneshot::channel();
    let (_release, release_rx) = oneshot::channel();
    let fake = Arc::new(Fake::new([Step::Wait(started, release_rx, 7)]));
    let model = Model::from_adapter("fake", "model", FakeAdapter(fake)).unwrap();
    let call = model.invoke_controlled(
        ModelRequest::new("hi"),
        None,
        Observations::new(&observer),
        ExecutionControl::new(&budget).with_cancellation(&token),
    );
    let cancel = async {
        started_rx.await.unwrap();
        token.cancel();
    };
    let (result, ()) = tokio::join!(call, cancel);
    assert!(matches!(result, Err(ModelError::Provider(error)) if error.is_cancelled()));
    assert_eq!(*observer.0.lock().unwrap(), [AttemptOutcome::Cancelled]);
    assert_eq!(budget.snapshot().in_flight, 0);
    let token = CancellationToken::new();
    let fake = Arc::new(Fake::new([Step::Cancel(token.clone())]));
    let model = Model::from_adapter("fake", "model", FakeAdapter(fake)).unwrap();
    let result = model
        .invoke_controlled(
            ModelRequest::new("hi"),
            None,
            Observations::default(),
            ExecutionControl::new(&budget).with_cancellation(&token),
        )
        .await;
    assert!(matches!(result, Err(ModelError::Provider(error)) if error.is_cancelled()));
    assert_eq!(budget.snapshot().tokens, 9);
}

#[tokio::test]
async fn sdk_stream_and_structured_calls_use_the_same_admission_boundary() {
    let fake = Arc::new(Fake::new([Step::Ok(7), Step::Partial(rate())]));
    let model = Model::from_adapter("fake", "model", FakeAdapter(fake.clone())).unwrap();
    let budget = limited(2);
    let observer = Observer::default();
    let mut request = ModelRequest::new("hi");
    request.schema = Some(serde_json::json!({"type":"object"}));
    let output = model
        .invoke_controlled(
            request,
            None,
            Observations::new(&observer),
            ExecutionControl::new(&budget),
        )
        .await
        .unwrap();
    assert!(output.structured.is_some());
    let (tx, mut rx) = mpsc::channel(1);
    let call = model.invoke_controlled(
        ModelRequest::new("hi"),
        Some(tx),
        Observations::new(&observer),
        ExecutionControl::new(&budget),
    );
    let drain = async {
        let mut text = String::new();
        while let Some(StreamToken::Text(token)) = rx.recv().await {
            text.push_str(&token);
        }
        text
    };
    let (result, text) = tokio::join!(call, drain);
    assert!(result.is_err());
    assert_eq!(text, "partial");
    assert_eq!(budget.snapshot().tokens, 7);
    assert_eq!(budget.snapshot().model_attempts, 2);
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
    assert_eq!(observer.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cancellation_interrupts_retry_backoff_without_another_attempt() {
    struct Signal(Mutex<Option<oneshot::Sender<()>>>);
    #[async_trait]
    impl InferenceObserver for Signal {
        async fn attempt(&self, _: AttemptObservation<'_>) -> Result<(), ObservationError> {
            if let Some(signal) = self.0.lock().unwrap().take() {
                let _ = signal.send(());
            }
            Ok(())
        }
    }
    let (sent, received) = oneshot::channel();
    let observer = Signal(Mutex::new(Some(sent)));
    let fake = Arc::new(Fake::new([Step::Fail(rate())]));
    let mut group = group(fake.clone());
    group.retry.initial_backoff_ms = 60_000;
    group.retry.max_backoff_ms = 60_000;
    let token = CancellationToken::new();
    let budget = limited(5);
    let call = group.inference_controlled(
        request(Observations::new(&observer)),
        ExecutionControl::new(&budget).with_cancellation(&token),
    );
    let cancel = async {
        received.await.unwrap();
        token.cancel();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(call, cancel)
    })
    .await
    .expect("cancellation should interrupt a 60s backoff");
    assert!(result.unwrap_err().is_cancelled());
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
}
