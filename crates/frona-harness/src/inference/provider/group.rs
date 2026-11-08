use crate::{DefaultBudget, ExecutionControl};
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
        let budget = DefaultBudget::default();
        self.inference_controlled(request, ExecutionControl::new(&budget))
            .await
    }

    pub async fn inference_controlled(
        &self,
        request: ModelRequest<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<ModelResponse, InferenceError> {
        let (execution, report) = execution.begin_model_call(&self.main);
        let result = retry::inference_with_retry_and_fallback(
            self,
            request,
            retry::HistoryOverflow::TruncateOldest,
            execution,
        )
        .await;
        if let Some(report) = report {
            report.finish_inference(&result);
        }
        result.map(|(content, usage)| ModelResponse { content, usage })
    }

    /// Generate without trimming the selected history, using the turn's shared controls.
    pub async fn inference_preserving_history_controlled(
        &self,
        request: ModelRequest<'_>,
        execution: ExecutionControl<'_>,
    ) -> Result<ModelResponse, InferenceError> {
        let (execution, report) = execution.begin_model_call(&self.main);
        let result = retry::inference_with_retry_and_fallback(
            self,
            request,
            retry::HistoryOverflow::Reject,
            execution,
        )
        .await;
        if let Some(report) = report {
            report.finish_inference(&result);
        }
        result.map(|(content, usage)| ModelResponse { content, usage })
    }

    pub async fn structured_inference(
        &self,
        request: ModelRequest<'_>,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value, InferenceError> {
        let budget = DefaultBudget::default();
        self.structured_inference_controlled(request, schema, ExecutionControl::new(&budget))
            .await
    }

    pub async fn structured_inference_controlled(
        &self,
        request: ModelRequest<'_>,
        schema: serde_json::Value,
        execution: ExecutionControl<'_>,
    ) -> Result<serde_json::Value, InferenceError> {
        let (execution, report) = execution.begin_model_call(&self.main);
        let result =
            retry::structured_inference_with_retry_and_fallback(self, request, schema, execution)
                .await;
        if let Some(report) = report {
            report.finish_inference(&result);
        }
        result
    }
}
