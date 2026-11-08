//! Prepared model groups must use the same SDK admission and conversation machinery.
use async_trait::async_trait;
use frona_harness::{
    BudgetLimits, DefaultBudget, ExecutionStop, Handle, Harness, ManualError, Model, UsageReport,
    config::{ProviderModel, RetryConfig},
    inference::{
        InferenceError, ModelConfig, Usage,
        observation::{InferenceObserver, ObservationError, UsageObservation},
        provider::{
            InferenceOutput, ModelProvider, ModelRequestSettings, StreamToken, group::ModelGroup,
        },
    },
};
use rig_core::completion::{
    AssistantContent, Message,
    message::{ToolCall, ToolFunction},
    request::ToolDefinition,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

type Calls = Arc<Mutex<Vec<(String, Option<u64>, Option<f64>, Vec<Message>)>>>;
struct Script {
    outputs: Mutex<VecDeque<Result<InferenceOutput, InferenceError>>>,
    calls: Calls,
}
#[async_trait]
impl ModelProvider for Script {
    async fn inference(
        &self,
        model: &ModelConfig,
        _: &str,
        messages: Vec<Message>,
        _: Vec<ToolDefinition>,
        max: Option<u64>,
        temperature: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        self.calls
            .lock()
            .unwrap()
            .push((model.as_str(), max, temperature, messages));
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected attempt")
    }
    async fn stream_inference(
        &self,
        _: &ModelConfig,
        _: &str,
        _: Vec<Message>,
        _: Vec<ToolDefinition>,
        _: tokio::sync::mpsc::Sender<StreamToken>,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<InferenceOutput, InferenceError> {
        unreachable!()
    }
    async fn structured_inference(
        &self,
        _: &ModelConfig,
        _: &str,
        _: Vec<Message>,
        _: serde_json::Value,
        _: Option<u64>,
        _: Option<f64>,
    ) -> Result<serde_json::Value, InferenceError> {
        unreachable!()
    }
}
fn group(outputs: Vec<Result<InferenceOutput, InferenceError>>) -> (ModelGroup, Calls) {
    let calls = Arc::new(Mutex::new(vec![]));
    let provider = Arc::new(Script {
        outputs: Mutex::new(outputs.into()),
        calls: calls.clone(),
    });
    let model = |id: &str, max, temperature| ModelConfig {
        catalog_provider: String::new(),
        provider_handle: Handle::const_validated("fixture"),
        model_id: id.into(),
        provider: ProviderModel::Generic,
        request_settings: ModelRequestSettings {
            max_tokens: Some(max),
            temperature: Some(temperature),
            ..Default::default()
        },
    };
    (
        ModelGroup {
            name: "prepared".into(),
            main: model("main", 400, 0.7),
            fallbacks: vec![model("backup", 200, 0.3)],
            max_tokens: Some(400),
            temperature: Some(0.7),
            context_window: 4096,
            retry: RetryConfig {
                max_retries: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
                backoff_multiplier: 1.0,
            },
            inference: Default::default(),
            providers: Arc::new([("fixture".into(), provider as Arc<dyn ModelProvider>)].into()),
        },
        calls,
    )
}
fn output(content: Vec<AssistantContent>) -> Result<InferenceOutput, InferenceError> {
    Ok(InferenceOutput::new(
        content,
        Usage {
            total_tokens: 7,
            ..Usage::default()
        },
    )
    .with_metadata(json!({"raw":"preserved"}))
    .with_ttft(Some(23)))
}
fn failed() -> Result<InferenceOutput, InferenceError> {
    Err(InferenceError::InferenceFailed("fixture failure".into()))
}
fn signed() -> AssistantContent {
    AssistantContent::ToolCall(
        ToolCall::from_wire("native", ToolFunction::new("read".into(), json!({})))
            .with_signature(Some("opaque".into())),
    )
}
#[derive(Clone)]
struct Observer(Arc<Mutex<Vec<(String, u8, u32)>>>);
#[async_trait]
impl InferenceObserver for Observer {
    async fn usage(&self, value: UsageObservation<'_>) -> Result<(), ObservationError> {
        self.0.lock().unwrap().push((
            value.model.model_id.clone(),
            value.fallback_index,
            value.latency.retry_count,
        ));
        Err("optional telemetry failed".into())
    }
}
#[tokio::test]
async fn prepared_group_preserves_retries_settings_source_and_optional_observations() {
    let (group, calls) = group(vec![
        Err(InferenceError::RateLimited {
            retry_after_secs: 0,
        }),
        failed(),
        output(vec![AssistantContent::text("done")]),
    ]);
    let observer = Observer(Arc::new(Mutex::new(vec![])));
    let harness = Harness::new(Model::from_group(group));
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(3),
        ..Default::default()
    });
    let mut session = harness
        .run("work")
        .budget(&budget)
        .observer(observer.clone())
        .manual();
    let report = session.report();
    session.call_model().await.unwrap();
    let response = &session.conversation().responses()[0];
    assert_eq!(response.origin(), Some("fixture/backup"));
    assert_eq!(response.output().metadata["raw"], "preserved");
    assert_eq!(response.output().ttft_ms, Some(23));
    assert!(matches!(
        response.output().usage,
        UsageReport::LegacyNormalized(_)
    ));
    assert_eq!(session.finish().unwrap(), "done");
    let report = report.snapshot();
    assert_eq!(report.requests.len(), 1);
    assert_eq!(report.attempts.len(), 3);
    assert_eq!(report.attempts[2].model, "fixture/backup");
    assert_eq!(*observer.0.lock().unwrap(), [("backup".into(), 1, 0)]);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|c| (c.1, c.2)).collect::<Vec<_>>(),
        [
            (Some(400), Some(0.7)),
            (Some(400), Some(0.7)),
            (Some(200), Some(0.3))
        ]
    );
}
#[tokio::test]
async fn fallback_cannot_receive_primary_native_history_after_primary_fails() {
    let (group, calls) = group(vec![
        output(vec![signed()]),
        failed(),
        output(vec![AssistantContent::text("must not run")]),
    ]);
    let harness = Harness::new(Model::from_group(group));
    let mut session = harness.run("work").manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    session.reject(action, "read result").unwrap();
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::Stop(ExecutionStop::Stopped(_)))
    ));
    assert_eq!(calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn fallback_native_origin_is_retained_and_cannot_be_replayed_as_primary() {
    let (group, calls) = group(vec![failed(), output(vec![signed()])]);
    let harness = Harness::new(Model::from_group(group));
    let mut session = harness.run("work").manual();
    let action = session.call_model().await.unwrap().actions.pop().unwrap();
    assert_eq!(
        session.conversation().responses()[0].origin(),
        Some("fixture/backup")
    );
    session.reject(action, "result").unwrap();
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::Incompatible { .. })
    ));
    assert_eq!(calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn shared_attempt_limit_prevents_fallback_without_readmitting_a_request() {
    let (group, calls) = group(vec![
        failed(),
        output(vec![AssistantContent::text("never")]),
    ]);
    let harness = Harness::new(Model::from_group(group));
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(1),
        ..Default::default()
    });
    let mut session = harness.run("work").budget(&budget).manual();
    let report = session.report();
    assert!(matches!(
        session.call_model().await,
        Err(ManualError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(report.snapshot().requests.len(), 1);
    assert_eq!(report.snapshot().attempts.len(), 1);
}

#[tokio::test]
async fn group_compaction_uses_prepared_window_and_keeps_summary_fallback_provenance() {
    use frona_harness::HistoryEvent;
    let history = compaction_history();
    let (mut group, calls) = group(vec![
        failed(),
        output(vec![AssistantContent::text("Earlier facts summarized.")]),
        output(vec![AssistantContent::text("done")]),
    ]);
    group.context_window = 1400;
    group.fallbacks[0].request_settings.max_tokens = Some(600);
    let observer = Observer(Arc::new(Mutex::new(vec![])));
    let harness = Harness::new(Model::from_group(group));
    let budget = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(3),
        ..Default::default()
    });
    let mut session = harness
        .resume(history)
        .budget(&budget)
        .observer(observer.clone())
        .manual();
    let report = session.report();
    session.recover().await.unwrap();
    session.call_model().await.unwrap();
    let compacted = session
        .conversation()
        .history()
        .iter()
        .find_map(|record| match &record.event {
            HistoryEvent::Compacted {
                summarizer, output, ..
            } => Some((summarizer, output)),
            _ => None,
        })
        .expect("the prepared 1400-token window should trigger compaction");
    assert_eq!(compacted.0, "fixture/backup");
    assert_eq!(compacted.1.metadata["raw"], "preserved");
    assert_eq!(compacted.1.ttft_ms, Some(23));
    assert_eq!(session.finish().unwrap(), "done");
    assert_eq!(report.snapshot().requests.len(), 2);
    assert_eq!(report.snapshot().attempts.len(), 3);
    assert_eq!(observer.0.lock().unwrap().len(), 2);
    assert_eq!(budget.snapshot().model_attempts, 3);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|c| c.1).collect::<Vec<_>>(),
        [Some(400), Some(600), Some(400)]
    );
    assert!(
        serde_json::to_string(&calls[2].3)
            .unwrap()
            .contains("Earlier facts summarized.")
    );
}

#[tokio::test]
async fn group_planning_reserves_the_largest_candidate_output_before_any_call() {
    let (mut group, calls) = group(vec![]);
    group.context_window = 500;
    group.fallbacks[0].request_settings.max_tokens = Some(600);
    let harness = Harness::new(Model::from_group(group));
    let incomplete = harness.run("work").await.unwrap_err();
    assert!(matches!(
        incomplete.reason,
        ManualError::Compaction(frona_harness::CompactionError::InvalidConfiguration)
    ));
    assert!(calls.lock().unwrap().is_empty());
}

fn compaction_history() -> frona_harness::Conversation {
    use frona_harness::{Conversation, ModelOutput};
    use rig_core::completion::message::UserContent;
    let mut history = Conversation::new("earlier facts ".repeat(110));
    history
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::text("earlier analysis ".repeat(100))],
                UsageReport::Unavailable,
            ),
            vec![],
        )
        .unwrap();
    history
        .push_user(vec![UserContent::text("Continue with the next task")])
        .unwrap();
    history
}

#[tokio::test]
async fn smaller_manual_group_uses_its_window_for_summary_and_shared_admission() {
    for limit in [1, 2] {
        let (initial, initial_calls) = group(vec![]);
        let (mut selected, selected_calls) = group(vec![
            output(vec![AssistantContent::text("Summary from selected group")]),
            output(vec![AssistantContent::text("done")]),
        ]);
        selected.context_window = 1400;
        selected.fallbacks[0].request_settings.max_tokens = Some(600);
        let selected = Model::from_group(selected);
        let harness = Harness::new(Model::from_group(initial));
        let budget = DefaultBudget::new(BudgetLimits {
            model_attempts: Some(limit),
            ..Default::default()
        });
        let mut session = harness
            .resume(compaction_history())
            .budget(&budget)
            .manual();
        let report = session.report();
        session.recover().await.unwrap();
        let result = session.call_model().model(&selected).await;
        assert!(session.conversation().compacted_view().is_some());
        if limit == 1 {
            assert!(matches!(
                result,
                Err(ManualError::Stop(ExecutionStop::BudgetExhausted(_)))
            ));
        } else {
            result.unwrap();
            assert_eq!(session.finish().unwrap(), "done");
        }
        assert!(initial_calls.lock().unwrap().is_empty());
        assert_eq!(selected_calls.lock().unwrap().len(), limit as usize);
        assert_eq!(budget.snapshot().model_attempts, limit);
        assert_eq!(report.snapshot().requests.len(), 2);
        assert_eq!(report.snapshot().attempts.len(), limit as usize);
    }
}

#[tokio::test]
async fn larger_manual_group_uses_its_window_but_preserves_explicit_compaction_policy() {
    use frona_harness::{CompactionConfig, CompactionError};
    for explicit in [false, true] {
        let (mut initial, initial_calls) = group(vec![]);
        initial.context_window = 500;
        let (mut selected, selected_calls) =
            group(vec![output(vec![AssistantContent::text("done")])]);
        selected.main.request_settings.max_tokens = Some(1000);
        let selected = Model::from_group(selected);
        let harness = Harness::new(Model::from_group(initial));
        let mut run = harness.run("work");
        if explicit {
            run = run.compaction(CompactionConfig {
                context_window: 500,
                ..Default::default()
            });
        }
        let mut session = run.manual();
        let result = session.call_model().model(&selected).await;
        if explicit {
            assert!(matches!(
                result,
                Err(ManualError::Compaction(
                    CompactionError::InvalidConfiguration
                ))
            ));
            assert!(selected_calls.lock().unwrap().is_empty());
        } else {
            result.unwrap();
            assert_eq!(session.finish().unwrap(), "done");
            assert_eq!(selected_calls.lock().unwrap()[0].1, Some(1000));
        }
        assert!(initial_calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn disabling_compaction_keeps_default_window_dynamic_and_prevents_summary_calls() {
    use frona_harness::CompactionError;
    let (initial, initial_calls) = group(vec![]);
    let (mut selected, selected_calls) = group(vec![]);
    selected.context_window = 900;
    let selected = Model::from_group(selected);
    let harness = Harness::new(Model::from_group(initial));
    let mut session = harness
        .resume(compaction_history())
        .disable_compaction()
        .manual();
    session.recover().await.unwrap();
    assert!(matches!(
        session.call_model().model(&selected).await,
        Err(ManualError::Compaction(
            CompactionError::ContextOverflow { .. }
        ))
    ));
    assert!(session.conversation().compacted_view().is_none());
    assert!(selected_calls.lock().unwrap().is_empty());
    assert!(initial_calls.lock().unwrap().is_empty());
}
