// Shared provider script used by portable and server execution-boundary tests.
// Each consumer uses a different subset of steps and helpers.
#![allow(dead_code)]

use async_trait::async_trait;
use frona_harness::{
    BudgetLimits, Capabilities, DefaultBudget, ExecutionControl, Handle, ModelAdapter, ModelOutput,
    ModelRequest, UncertainUsagePolicy, UsageReport,
    config::ProviderModel,
    inference::{
        InferenceError, ModelGroup, Usage,
        observation::{
            AttemptObservation, AttemptOutcome, InferenceObserver, ObservationError, Observations,
        },
        provider::{ModelConfig, ModelProvider, ModelRequestSettings, ResolvedModel, StreamToken},
    },
    retry_config::RetryConfig,
};
use futures::StreamExt;
use rig_core::completion::{AssistantContent, Message};
use rig_core::{
    driver::{Opened, Opening},
    test_utils::{MockFrame, MockStreamEvent},
    wire::Mode,
};
#[path = "rig.rs"]
mod rig;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub(crate) fn usage(tokens: u64) -> Usage {
    Usage::new().total_tokens(tokens)
}

// Native streams buffer their first 64 characters while detecting textual tool calls.
pub(crate) const NATIVE_PARTIAL: &str =
    "This partial answer exceeds the initial sixty-four character detection buffer.";

pub(crate) enum Step {
    Ok(u64),
    Fail(InferenceError),
    Partial(InferenceError),
    Wait(oneshot::Sender<()>, oneshot::Receiver<()>, u64),
    Cancel(CancellationToken),
}
pub(crate) struct Fake {
    steps: Arc<Mutex<VecDeque<Step>>>,
    pub(crate) calls: Arc<Mutex<Vec<String>>>,
}
impl Fake {
    pub(crate) fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
    async fn call(
        &self,
        name: &str,
        stream: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.calls.lock().unwrap().push(name.into());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider attempt");
        let tokens = match step {
            Step::Ok(tokens) => tokens,
            Step::Fail(error) => return Err(error),
            Step::Partial(error) => {
                stream
                    .unwrap()
                    .send(StreamToken::Text("partial".into()))
                    .await
                    .unwrap();
                return Err(error);
            }
            Step::Wait(started, release, tokens) => {
                if let Some(stream) = stream {
                    stream
                        .send(StreamToken::Text("partial".into()))
                        .await
                        .unwrap();
                }
                let _ = started.send(());
                let _ = release.await;
                tokens
            }
            Step::Cancel(token) => {
                token.cancel();
                9
            }
        };
        let mut output = ModelOutput::new(
            vec![AssistantContent::text("ok")],
            UsageReport::Reported(usage(tokens)),
        );
        output.structured = Some(serde_json::json!({"answer":"ok"}));
        Ok(output)
    }
}
pub(crate) struct FakeAdapter(pub(crate) Arc<Fake>);
#[async_trait]
impl ModelAdapter for FakeAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured: true,
            streaming: true,
            ..Capabilities::default()
        }
    }
    async fn generate(
        &self,
        _: ModelRequest,
        tokens: Option<mpsc::Sender<StreamToken>>,
    ) -> Result<ModelOutput, InferenceError> {
        self.0.call("sdk", tokens).await
    }
}
#[async_trait]
impl ModelProvider for Fake {
    async fn resolve(&self, model: &ModelConfig) -> Result<ResolvedModel, InferenceError> {
        self.calls.lock().unwrap().push(model.model_id.clone());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider attempt");
        if let Step::Fail(error) = step {
            return Err(error);
        }
        let step = Mutex::new(Some(step));
        Ok(ResolvedModel::new(rig::model(
            model.model_id.clone(),
            move |request, mode| {
                let step = step
                    .lock()
                    .unwrap()
                    .take()
                    .expect("one invocation per resolution");
                let structured = request.tools.iter().any(|tool| tool.name == "submit");
                let output = move |tokens| {
                    let content = if structured {
                        vec![AssistantContent::ToolCall(
                            rig_core::message::ToolCall::from_wire(
                                "submission",
                                rig_core::message::ToolFunction::new(
                                    "submit".try_into().unwrap(),
                                    serde_json::json!({"answer":"ok"}),
                                ),
                            ),
                        )]
                    } else {
                        vec![AssistantContent::text("ok")]
                    };
                    ModelOutput::new(content, UsageReport::Reported(usage(tokens)))
                };
                match step {
                    Step::Ok(tokens) => rig::reply(output(tokens)),
                    Step::Partial(error) => Opening::ready(Opened::new(futures::stream::iter([
                        Ok(MockFrame::Event(MockStreamEvent::Text(
                            NATIVE_PARTIAL.into(),
                        ))),
                        Err(rig::error(error)),
                    ]))),
                    Step::Wait(started, release, tokens) => {
                        let prefix = if matches!(mode, Mode::Streaming) {
                            vec![Ok(MockFrame::Event(MockStreamEvent::Text(
                                NATIVE_PARTIAL.into(),
                            )))]
                        } else {
                            vec![]
                        };
                        Opening::ready(Opened::new(futures::stream::iter(prefix).chain(
                            futures::stream::once(async move {
                                let _ = started.send(());
                                let _ = release.await;
                                Ok(MockFrame::Response(Box::new(rig::response(output(tokens)))))
                            }),
                        )))
                    }
                    Step::Cancel(token) => Opening::new(async move {
                        token.cancel();
                        Ok(Opened::new(futures::stream::iter([Ok(
                            MockFrame::Response(Box::new(rig::response(output(9)))),
                        )])))
                    }),
                    Step::Fail(_) => unreachable!(),
                }
            },
        )))
    }
}
pub(crate) fn group(fake: Arc<Fake>) -> ModelGroup {
    let config = |name: &str| ModelConfig {
        catalog_provider: "fake".into(),
        provider_handle: Handle::const_validated("fake"),
        model_id: name.into(),
        provider: ProviderModel::Generic,
        request_settings: ModelRequestSettings::default(),
    };
    let providers: HashMap<String, Arc<dyn ModelProvider>> =
        [("fake".into(), fake as Arc<dyn ModelProvider>)].into();
    ModelGroup {
        name: "test".into(),
        main: config("primary"),
        fallbacks: vec![config("fallback")],
        max_tokens: None,
        temperature: None,
        context_window: 100_000,
        retry: RetryConfig {
            max_retries: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            ..Default::default()
        },
        inference: Default::default(),
        providers: Arc::new(providers),
    }
}
#[derive(Default)]
pub(crate) struct Observer(pub(crate) Mutex<Vec<AttemptOutcome>>);
#[async_trait]
impl InferenceObserver for Observer {
    async fn attempt(&self, observation: AttemptObservation<'_>) -> Result<(), ObservationError> {
        self.0.lock().unwrap().push(observation.outcome);
        // Deliberate telemetry failure cannot suppress reliable accounting.
        Err("telemetry unavailable".into())
    }
}
pub(crate) fn request(
    observations: Observations<'_>,
) -> frona_harness::inference::ModelRequest<'_> {
    frona_harness::inference::ModelRequest {
        input_token_estimate: None,
        system_prompt: "",
        history: vec![Message::user("hi")],
        tools: vec![],
        observations,
        overrides: Default::default(),
    }
}
pub(crate) fn limited(attempts: u64) -> DefaultBudget {
    DefaultBudget::new(BudgetLimits {
        model_attempts: Some(attempts),
        uncertain_usage: UncertainUsagePolicy::KnownOnly,
        ..Default::default()
    })
}
pub(crate) fn rate() -> InferenceError {
    InferenceError::RateLimited {
        retry_after_secs: 0,
    }
}
pub(crate) async fn run(
    group: &ModelGroup,
    structured: bool,
    control: ExecutionControl<'_>,
    observer: &Observer,
) -> Result<(), InferenceError> {
    if structured {
        group
            .structured_inference_controlled(
                request(Observations::new(observer)),
                serde_json::json!({"type":"object"}),
                control,
            )
            .await
            .map(|_| ())
    } else {
        group
            .inference_controlled(request(Observations::new(observer)), control)
            .await
            .map(|_| ())
    }
}
