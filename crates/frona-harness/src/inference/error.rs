use thiserror::Error;

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error(transparent)]
    Stop(#[from] crate::ExecutionStop),

    #[error("Provider not configured: {0}")]
    ProviderNotConfigured(String),

    #[error("Model group not found: {0}")]
    ModelGroupNotFound(String),

    #[error("Inference failed: {0}")]
    InferenceFailed(String),

    #[error("Streaming failed: {0}")]
    StreamingFailed(String),

    #[error("Completion error: {0}")]
    CompletionFailed(#[source] Box<rig_core::error::ProviderError>),

    #[error("All fallbacks failed: {}", format_fallback_errors(.0))]
    AllFallbacksFailed(Vec<InferenceError>),

    #[error("{provider}/{model}: {source}")]
    ModelFailed {
        provider: String,
        model: String,
        retry_count: u32,
        #[source]
        source: Box<InferenceError>,
    },

    #[error("Rate limited: retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Empty response from model")]
    EmptyResponse,

    #[error("Cancelled")]
    Cancelled(String),

    #[error("Config error: {0}")]
    ConfigError(String),
}

impl From<rig_core::error::ProviderError> for InferenceError {
    fn from(error: rig_core::error::ProviderError) -> Self {
        Self::CompletionFailed(Box::new(error))
    }
}

fn has_non_retryable_status(msg: &str) -> bool {
    ["400", "401", "403", "404", "405", "422"]
        .into_iter()
        .any(|code| msg.contains(code))
}

impl InferenceError {
    /// Terminal controls must leave the whole retry/fallback orchestration.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Stop(_) | Self::Cancelled(_) => true,
            Self::ModelFailed { source, .. } => source.is_terminal(),
            Self::AllFallbacksFailed(errors) => errors.iter().any(Self::is_terminal),
            _ => false,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Stop(crate::ExecutionStop::Cancelled(_)) | Self::Cancelled(_) => true,
            Self::ModelFailed { source, .. } => source.is_cancelled(),
            Self::AllFallbacksFailed(errors) => errors.iter().any(Self::is_cancelled),
            _ => false,
        }
    }

    /// Classify nested terminal controls without losing provider provenance.
    pub(crate) fn execution_failure_kind(&self) -> Option<crate::FailureKind> {
        match self {
            Self::Stop(stop) => Some(stop.failure_kind()),
            Self::Cancelled(_) => Some(crate::FailureKind::Cancelled),
            Self::ModelFailed { source, .. } => source.execution_failure_kind(),
            Self::AllFallbacksFailed(errors) => {
                errors.iter().find_map(Self::execution_failure_kind)
            }
            _ => None,
        }
    }

    /// Extract a control without string matching, retaining ordinary failures.
    pub fn into_execution_stop(self) -> Result<crate::ExecutionStop, Self> {
        if !self.is_terminal() {
            return Err(self);
        }
        match self {
            Self::Stop(stop) => Ok(stop),
            Self::Cancelled(reason) => Ok(crate::ExecutionStop::Cancelled(reason)),
            Self::ModelFailed { source, .. } => source.into_execution_stop(),
            Self::AllFallbacksFailed(mut errors) => {
                let index = errors
                    .iter()
                    .position(Self::is_terminal)
                    .expect("terminal error exists");
                errors.remove(index).into_execution_stop()
            }
            _ => unreachable!("terminal classification is exhaustive"),
        }
    }

    pub fn for_model(
        self,
        model: &crate::inference::provider::ModelConfig,
        retry_count: u32,
    ) -> Self {
        Self::ModelFailed {
            provider: model.provider_name().into(),
            model: model.model_id.clone(),
            retry_count,
            source: Box::new(self),
        }
    }

    pub fn is_retryable(&self) -> bool {
        if self.is_terminal() {
            return false;
        }
        match self {
            Self::ModelFailed { source, .. } => source.is_retryable(),
            InferenceError::RateLimited { .. } | InferenceError::EmptyResponse => true,
            InferenceError::Cancelled(_) => false,
            InferenceError::CompletionFailed(error) => {
                use rig_core::error::ProviderError;
                if let Some(status) = error.provider_response_status() {
                    let code = status.as_u16();
                    return status.is_success() || matches!(code, 429 | 500 | 502 | 503 | 504);
                }
                match error.as_ref() {
                    ProviderError::Http(error) => {
                        matches!(
                            &**error,
                            rig_core::http_client::Error::Instance(_)
                                | rig_core::http_client::Error::StreamEnded
                        )
                    }
                    ProviderError::Json(_)
                    | ProviderError::Truncated
                    | ProviderError::Stalled(_) => true,
                    ProviderError::Provider(message) => !has_non_retryable_status(message),
                    _ => false,
                }
            }
            InferenceError::InferenceFailed(msg) | InferenceError::StreamingFailed(msg) => {
                let lower = msg.to_lowercase();
                lower.contains("429") || lower.contains("timeout") || lower.contains("overloaded")
            }
            _ => false,
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        match self {
            Self::ModelFailed { source, .. } => source.is_rate_limited(),
            InferenceError::RateLimited { .. } => true,
            InferenceError::CompletionFailed(error) => {
                error
                    .provider_response_status()
                    .is_some_and(|status| status.as_u16() == 429)
                    || matches!(error.as_ref(), rig_core::error::ProviderError::Provider(message)
                        if message.contains("429"))
            }
            _ => false,
        }
    }

    pub fn retry_reason(&self) -> &'static str {
        if let Self::ModelFailed { source, .. } = self {
            return source.retry_reason();
        }
        if self.is_rate_limited() {
            return "rate_limited";
        }
        match self {
            InferenceError::EmptyResponse => "empty_response",
            InferenceError::CompletionFailed(error) => {
                use rig_core::error::ProviderError;
                if error
                    .provider_response_status()
                    .is_some_and(|status| status.is_success())
                    || matches!(error.as_ref(), ProviderError::Json(_))
                {
                    "invalid_provider_response"
                } else if matches!(error.as_ref(), ProviderError::Http(_)) {
                    "network_error"
                } else if matches!(error.as_ref(), ProviderError::Stalled(_)) {
                    "timeout"
                } else {
                    "server_error"
                }
            }
            InferenceError::StreamingFailed(msg) if msg.to_lowercase().contains("timeout") => {
                "timeout"
            }
            InferenceError::InferenceFailed(msg) if msg.to_lowercase().contains("overloaded") => {
                "overloaded"
            }
            _ => "server_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use rig_core::ProviderResponseError;
    use rig_core::error::ProviderError;

    use crate::inference::InferenceError;

    #[test]
    fn a_successful_http_response_that_rig_cannot_decode_is_retryable() {
        let error = InferenceError::from(ProviderError::ProviderResponse(
            ProviderResponseError::new(http::StatusCode::OK, r#"{"choices":[]}"#),
        ));

        assert!(error.is_retryable());
        assert_eq!(error.retry_reason(), "invalid_provider_response");
    }

    #[test]
    fn malformed_provider_json_is_retryable() {
        let json_error =
            serde_json::from_str::<serde_json::Value>(r#"{"output":"cut off"#).unwrap_err();
        let error = InferenceError::from(ProviderError::from(json_error));

        assert!(error.is_retryable());
        assert_eq!(error.retry_reason(), "invalid_provider_response");
    }

    #[test]
    fn a_provider_response_without_an_http_success_status_is_not_retryable() {
        let error = InferenceError::from(ProviderError::ProviderResponse(
            ProviderResponseError::without_status("unknown provider failure"),
        ));

        assert!(!error.is_retryable());
    }
}

fn format_fallback_errors(errors: &[InferenceError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}
