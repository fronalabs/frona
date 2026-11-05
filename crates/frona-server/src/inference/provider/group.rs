use std::{collections::HashMap, sync::Arc};

use rig_core::completion::request::ToolDefinition;
use rig_core::completion::{AssistantContent, Message};

use crate::inference::{
    Usage,
    config::{InferenceConfig, RetryConfig},
    error::InferenceError,
    observation::Observations,
    provider::{ModelConfig, ModelProvider},
    retry,
};

/// Immutable execution configuration bound to its prepared provider clients.
#[derive(Clone)]
pub struct ModelGroup {
    pub name: String,
    pub main: ModelConfig,
    pub fallbacks: Vec<ModelConfig>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub context_window: usize,
    pub retry: RetryConfig,
    pub inference: InferenceConfig,
    pub providers: Arc<HashMap<String, Arc<dyn ModelProvider>>>,
}

impl std::fmt::Debug for ModelGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelGroup")
            .field("name", &self.name)
            .field("main", &self.main)
            .field("fallbacks", &self.fallbacks)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RequestOverrides {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
}

pub struct ModelRequest<'a> {
    pub input_token_estimate: Option<crate::inference::context::InputTokenEstimate>,
    pub system_prompt: &'a str,
    pub history: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub observations: Observations<'a>,
    pub overrides: RequestOverrides,
}

#[derive(Debug)]
pub struct ModelResponse {
    pub content: Vec<AssistantContent>,
    pub usage: Usage,
}

impl ModelGroup {
    pub async fn inference(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, InferenceError> {
        let (content, usage) = retry::inference_with_retry_and_fallback(
            self,
            request,
            retry::HistoryOverflow::TruncateOldest,
        )
        .await?;
        Ok(ModelResponse { content, usage })
    }

    /// Generate a completion without trimming any of its input. Each configured
    /// candidate must fit the whole request; retry, fallback and usage accounting
    /// otherwise follow the normal completion path.
    pub(crate) async fn inference_preserving_history(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, InferenceError> {
        let (content, usage) =
            retry::inference_with_retry_and_fallback(self, request, retry::HistoryOverflow::Reject)
                .await?;
        Ok(ModelResponse { content, usage })
    }

    pub async fn structured_inference(
        &self,
        request: ModelRequest<'_>,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value, InferenceError> {
        retry::structured_inference_with_retry_and_fallback(self, request, schema).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Handle, config::ProviderModel};
    use crate::inference::{
        ModelConfig,
        config::RetryConfig,
        observation::{
            AttemptObservation, AttemptOutcome, InferenceObserver, ObservationError,
            UsageObservation,
        },
        provider::{InferenceOutput, ModelRequestSettings, StreamToken},
    };
    use crate::{
        chat::broadcast::EventSender,
        inference::streaming::{ModelGroupStreaming, StreamResult},
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    type RecordedCall = (String, Option<u64>, Option<f64>);
    #[derive(Default)]
    struct RecordingProvider {
        calls: Mutex<Vec<RecordedCall>>,
        retry_failures: AtomicUsize,
    }

    impl RecordingProvider {
        fn record(
            &self,
            model: &ModelConfig,
            max: Option<u64>,
            temperature: Option<f64>,
        ) -> Result<(), InferenceError> {
            self.calls
                .lock()
                .unwrap()
                .push((model.model_id.clone(), max, temperature));
            if self
                .retry_failures
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(InferenceError::RateLimited {
                    retry_after_secs: 0,
                });
            }
            if model.model_id == "main" {
                Err(InferenceError::InferenceFailed("fixture failure".into()))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for RecordingProvider {
        async fn inference(
            &self,
            model: &ModelConfig,
            _: &str,
            _: Vec<Message>,
            _: Vec<ToolDefinition>,
            max: Option<u64>,
            temperature: Option<f64>,
        ) -> Result<InferenceOutput, InferenceError> {
            self.record(model, max, temperature)?;
            Ok(InferenceOutput::new(
                vec![AssistantContent::text("ok")],
                Usage::default(),
            ))
        }

        async fn stream_inference(
            &self,
            model: &ModelConfig,
            _: &str,
            _: Vec<Message>,
            _: Vec<ToolDefinition>,
            tokens: tokio::sync::mpsc::Sender<StreamToken>,
            max: Option<u64>,
            temperature: Option<f64>,
        ) -> Result<InferenceOutput, InferenceError> {
            self.record(model, max, temperature)?;
            tokens.send(StreamToken::Text("ok".into())).await.unwrap();
            Ok(InferenceOutput::new(
                vec![AssistantContent::text("ok")],
                Usage::default(),
            ))
        }

        async fn structured_inference(
            &self,
            model: &ModelConfig,
            _: &str,
            _: Vec<Message>,
            _: serde_json::Value,
            max: Option<u64>,
            temperature: Option<f64>,
        ) -> Result<serde_json::Value, InferenceError> {
            self.record(model, max, temperature)?;
            Ok(serde_json::json!({"answer": "ok"}))
        }
    }

    fn group(provider: Arc<RecordingProvider>) -> ModelGroup {
        let model = |id: &str, max, temperature| ModelConfig {
            catalog_provider: String::new(),
            provider_handle: Handle::const_validated("fixture").into(),
            model_id: id.into(),
            provider: ProviderModel::Generic,
            request_settings: ModelRequestSettings {
                max_tokens: Some(max),
                temperature: Some(temperature),
                ..Default::default()
            },
        };
        ModelGroup {
            name: "primary".into(),
            main: model("main", 400, 0.7),
            fallbacks: vec![model("backup", 200, 0.3)],
            max_tokens: Some(400),
            temperature: Some(0.7),
            context_window: 4096,
            retry: RetryConfig {
                max_retries: 0,
                ..Default::default()
            },
            inference: Default::default(),
            providers: Arc::new([("fixture".into(), provider as Arc<dyn ModelProvider>)].into()),
        }
    }

    #[derive(Debug)]
    struct RecordedUsage {
        model: String,
        tokens: Option<u64>,
        fallback: u8,
        retries: u32,
        attribution: Vec<(String, String)>,
    }

    #[derive(Default)]
    struct RecordingObserver {
        usage: Mutex<Vec<RecordedUsage>>,
        attempts: Mutex<Vec<(String, AttemptOutcome, Option<u64>)>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl InferenceObserver for RecordingObserver {
        async fn usage(&self, observation: UsageObservation<'_>) -> Result<(), ObservationError> {
            self.usage.lock().unwrap().push(RecordedUsage {
                model: observation.model.model_id.clone(),
                tokens: observation.usage.map(|usage| usage.total_tokens),
                fallback: observation.fallback_index,
                retries: observation.latency.retry_count,
                attribution: observation
                    .attribution
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            });
            if self.fail {
                Err(std::io::Error::other("telemetry unavailable").into())
            } else {
                Ok(())
            }
        }

        async fn attempt(
            &self,
            observation: AttemptObservation<'_>,
        ) -> Result<(), ObservationError> {
            self.attempts.lock().unwrap().push((
                observation.model.model_id.clone(),
                observation.outcome,
                observation.usage.map(|usage| usage.total_tokens),
            ));
            if self.fail {
                Err(std::io::Error::other("telemetry unavailable").into())
            } else {
                Ok(())
            }
        }
    }

    fn request(observations: Observations<'_>, overrides: RequestOverrides) -> ModelRequest<'_> {
        ModelRequest {
            input_token_estimate: None,
            system_prompt: "fixture",
            history: vec![Message::user("hello")],
            tools: vec![],
            observations,
            overrides,
        }
    }

    #[tokio::test]
    async fn all_execution_modes_share_fallbacks_and_per_request_overrides_are_isolated() {
        let observer = RecordingObserver::default();
        let tags = [("job", "standalone")];
        let observations = Observations::new(&observer).with_attribution(&tags);
        let provider = Arc::new(RecordingProvider::default());
        let group = group(provider.clone());
        let (first, second) = tokio::join!(
            group.inference(request(
                observations,
                RequestOverrides {
                    max_tokens: Some(100),
                    temperature: None
                }
            )),
            group.inference(request(
                observations,
                RequestOverrides {
                    max_tokens: Some(150),
                    temperature: Some(0.1)
                }
            )),
        );
        first.unwrap();
        second.unwrap();
        group
            .structured_inference(
                request(observations, RequestOverrides::default()),
                serde_json::json!({"type":"object"}),
            )
            .await
            .unwrap();
        let mut accumulated = String::new();
        let result = group
            .stream_inference(
                request(observations, RequestOverrides::default()),
                &EventSender::noop(),
                &tokio_util::sync::CancellationToken::new(),
                &mut accumulated,
            )
            .await
            .unwrap();
        assert!(matches!(result, StreamResult::Contents { .. }));
        assert_eq!(accumulated, "ok");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 8);
        assert!(calls.contains(&("backup".into(), Some(100), Some(0.3))));
        assert!(calls.contains(&("backup".into(), Some(150), Some(0.1))));
        assert_eq!(
            calls
                .iter()
                .filter(|call| **call == ("backup".into(), Some(200), Some(0.3)))
                .count(),
            2
        );
        assert_eq!(group.main.request_settings.max_tokens, Some(400));
        assert_eq!(group.fallbacks[0].request_settings.temperature, Some(0.3));
        let usages = observer.usage.lock().unwrap();
        assert_eq!(usages.len(), 4);
        assert_eq!(
            usages
                .iter()
                .filter(|usage| usage.tokens == Some(0))
                .count(),
            3
        );
        assert_eq!(
            usages.iter().filter(|usage| usage.tokens.is_none()).count(),
            1
        );
        assert!(
            usages
                .iter()
                .all(|usage| usage.model == "backup" && usage.fallback == 1 && usage.retries == 0)
        );
        assert!(
            usages
                .iter()
                .all(|usage| usage.attribution == [("job".into(), "standalone".into())])
        );
        let attempts = observer.attempts.lock().unwrap();
        assert_eq!(attempts.len(), 8);
        assert_eq!(
            attempts
                .iter()
                .filter(|(_, outcome, _)| *outcome == AttemptOutcome::Failed)
                .count(),
            4
        );
        assert_eq!(
            attempts
                .iter()
                .filter(|(_, outcome, _)| *outcome == AttemptOutcome::Succeeded)
                .count(),
            4
        );
    }
    #[tokio::test]
    async fn local_observers_see_each_retry_and_failures_do_not_change_execution() {
        let observer = RecordingObserver {
            fail: true,
            ..Default::default()
        };
        let provider = Arc::new(RecordingProvider::default());
        provider.retry_failures.store(2, Ordering::Relaxed);
        let mut group = group(provider.clone());
        group.main.model_id = "backup".into();
        group.fallbacks.clear();
        group.retry = RetryConfig {
            max_retries: 3,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            backoff_multiplier: 1.0,
        };
        group
            .inference(request(
                Observations::new(&observer),
                RequestOverrides::default(),
            ))
            .await
            .unwrap();
        assert_eq!(provider.calls.lock().unwrap().len(), 3);
        assert_eq!(
            observer
                .attempts
                .lock()
                .unwrap()
                .iter()
                .map(|(_, outcome, _)| *outcome)
                .collect::<Vec<_>>(),
            [
                AttemptOutcome::Failed,
                AttemptOutcome::Failed,
                AttemptOutcome::Succeeded
            ]
        );
        assert_eq!(observer.usage.lock().unwrap()[0].retries, 2);
        // A completely absent observer also needs no service or records.
        group
            .inference(request(
                Observations::default(),
                RequestOverrides::default(),
            ))
            .await
            .unwrap();
    }

    #[test]
    fn activity_counter_works_locally_and_broadcasting_is_optional() {
        let counts = Arc::new(Mutex::new(Vec::new()));
        let recorded = counts.clone();
        let counter = crate::inference::provider::InferenceCounter::new(move |n| {
            recorded.lock().unwrap().push(n)
        });
        let first = counter.guard();
        let second = counter.clone().guard();
        drop(first);
        drop(second);
        assert_eq!(*counts.lock().unwrap(), [1, 2, 1, 0]);
        let unobserved = crate::inference::provider::InferenceCounter::default();
        drop(unobserved.guard());
    }
}
