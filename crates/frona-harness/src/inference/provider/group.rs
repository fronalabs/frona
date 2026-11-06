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
    /// Return the prepared transport for one configured model.
    pub fn provider(&self, model: &ModelConfig) -> Result<Arc<dyn ModelProvider>, InferenceError> {
        retry::provider_for(&self.providers, model)
    }

    /// Check the complete input without truncating it. Calibrated estimates
    /// are only used when their request fingerprint matches.
    pub fn validate_complete_input(
        &self,
        history: &[Message],
        system_prompt: &str,
        tools: &[ToolDefinition],
        max_output: usize,
        estimate: Option<crate::inference::context::InputTokenEstimate>,
    ) -> Result<(), InferenceError> {
        retry::completion_history(
            history,
            system_prompt,
            tools,
            self,
            max_output,
            retry::HistoryOverflow::Reject,
            estimate,
        )
        .map(|_| ())
        .map_err(InferenceError::InferenceFailed)
    }

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
    pub async fn inference_preserving_history(
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
