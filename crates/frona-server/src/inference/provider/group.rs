use std::{collections::HashMap, sync::Arc};

use rig_core::completion::request::ToolDefinition;
use rig_core::completion::{AssistantContent, Message};

use crate::chat::broadcast::EventSender;
use crate::core::error::AppError;
use crate::inference::{
    Usage,
    config::{InferenceConfig, RetryConfig},
    error::InferenceError,
    provider::{ModelConfig, ModelProvider},
    retry::{self, StreamResult},
    usage::{UsageContext, UsageService},
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
    pub system_prompt: &'a str,
    pub history: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub usage_service: &'a UsageService,
    pub usage_context: &'a UsageContext,
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
            request.overrides,
            request.system_prompt,
            request.history,
            request.tools,
            request.usage_service,
            request.usage_context,
        )
        .await?;
        Ok(ModelResponse { content, usage })
    }

    pub async fn structured_inference(
        &self,
        request: ModelRequest<'_>,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value, InferenceError> {
        retry::structured_inference_with_retry_and_fallback(
            self,
            request.overrides,
            request.system_prompt,
            request.history,
            schema,
            request.usage_service,
            request.usage_context,
        )
        .await
    }

    pub async fn stream_inference(
        &self,
        request: ModelRequest<'_>,
        events: &EventSender,
        cancellation: &tokio_util::sync::CancellationToken,
        accumulated_text: &mut String,
    ) -> Result<StreamResult, AppError> {
        retry::stream_with_retry_and_fallback(
            self,
            request.overrides,
            request.system_prompt,
            &request.history,
            &request.tools,
            events,
            cancellation,
            accumulated_text,
            request.usage_service,
            request.usage_context,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Handle, config::ProviderModel};
    use crate::inference::{
        InferenceKind, ModelConfig, UsageContext,
        config::RetryConfig,
        provider::{InferenceOutput, ModelRequestSettings, StreamToken},
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingProvider {
        calls: Mutex<Vec<(String, Option<u64>, Option<f64>)>>,
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
            provider_handle: Handle::const_validated("fixture"),
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

    fn request<'a>(
        usage: &'a UsageService,
        context: &'a UsageContext,
        overrides: RequestOverrides,
    ) -> ModelRequest<'a> {
        ModelRequest {
            system_prompt: "fixture",
            history: vec![Message::user("hello")],
            tools: vec![],
            usage_service: usage,
            usage_context: context,
            overrides,
        }
    }

    #[tokio::test]
    async fn all_execution_modes_share_fallbacks_and_per_request_overrides_are_isolated() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let fixture = crate::app_state_fixture::build(&db).await;
        let usage = &fixture.state.usage_service;
        let context = UsageContext::new(
            InferenceKind::Text {
                agent_id: "agent".into(),
                chat_id: "chat".into(),
                message_id: "message".into(),
            },
            "user",
            "primary",
        );
        let provider = Arc::new(RecordingProvider::default());
        let group = group(provider.clone());
        let (first, second) = tokio::join!(
            group.inference(request(
                usage,
                &context,
                RequestOverrides {
                    max_tokens: Some(100),
                    temperature: None
                }
            )),
            group.inference(request(
                usage,
                &context,
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
                request(usage, &context, RequestOverrides::default()),
                serde_json::json!({"type":"object"}),
            )
            .await
            .unwrap();
        let mut accumulated = String::new();
        let result = group
            .stream_inference(
                request(usage, &context, RequestOverrides::default()),
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
    }
}
