use thiserror::Error;

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("Provider not configured: {0}")]
    ProviderNotConfigured(String),

    #[error("Model group not found: {0}")]
    ModelGroupNotFound(String),

    #[error("Inference failed: {0}")]
    InferenceFailed(String),

    #[error("Streaming failed: {0}")]
    StreamingFailed(String),

    #[error("Completion error: {0}")]
    CompletionFailed(#[from] rig_core::completion::CompletionError),

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

fn provider_error_contains_status(msg: &str, codes: &[u16]) -> bool {
    codes.iter().any(|code| msg.contains(&code.to_string()))
}

fn has_non_retryable_status(msg: &str) -> bool {
    let non_retryable: &[u16] = &[400, 401, 403, 404, 405, 422];
    non_retryable
        .iter()
        .any(|code| msg.contains(&code.to_string()))
}

impl InferenceError {
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
        match self {
            Self::ModelFailed { source, .. } => source.is_retryable(),
            InferenceError::RateLimited { .. } | InferenceError::EmptyResponse => true,
            InferenceError::Cancelled(_) => false,
            InferenceError::CompletionFailed(rig_core::completion::CompletionError::HttpError(
                http_err,
            )) => {
                use rig_core::http_client::Error;
                match http_err {
                    Error::InvalidStatusCode(s) | Error::InvalidStatusCodeWithMessage(s, _) => {
                        let code = s.as_u16();
                        code == 429 || code == 500 || code == 502 || code == 503 || code == 504
                    }
                    Error::Instance(_) => true,
                    _ => false,
                }
            }
            InferenceError::CompletionFailed(
                rig_core::completion::CompletionError::ProviderError(msg),
            ) => !has_non_retryable_status(msg),
            // A 2xx provider response that Rig cannot deserialize is usually tied to
            // the model's generated response shape. Retrying can produce a decodable
            // completion, while HTTP error statuses continue through the explicit
            // status-code policy above.
            InferenceError::CompletionFailed(
                rig_core::completion::CompletionError::ProviderResponse(response),
            ) => response.status.is_some_and(|status| status.is_success()),
            InferenceError::CompletionFailed(rig_core::completion::CompletionError::JsonError(
                _,
            )) => true,
            InferenceError::CompletionFailed(_) => false,
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
            InferenceError::CompletionFailed(rig_core::completion::CompletionError::HttpError(
                http_err,
            )) => {
                use rig_core::http_client::Error;
                matches!(
                    http_err,
                    Error::InvalidStatusCode(s) | Error::InvalidStatusCodeWithMessage(s, _)
                    if s.as_u16() == 429
                )
            }
            InferenceError::CompletionFailed(
                rig_core::completion::CompletionError::ProviderError(msg),
            ) => provider_error_contains_status(msg, &[429]),
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
            InferenceError::CompletionFailed(rig_core::completion::CompletionError::HttpError(
                rig_core::http_client::Error::Instance(_),
            )) => "network_error",
            InferenceError::CompletionFailed(rig_core::completion::CompletionError::HttpError(
                _,
            )) => "server_error",
            InferenceError::CompletionFailed(
                rig_core::completion::CompletionError::ProviderResponse(response),
            ) if response.status.is_some_and(|status| status.is_success()) => {
                "invalid_provider_response"
            }
            InferenceError::CompletionFailed(rig_core::completion::CompletionError::JsonError(
                _,
            )) => "invalid_provider_response",
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
    use rig_core::completion::CompletionError;

    use crate::inference::InferenceError;

    #[test]
    fn a_successful_http_response_that_rig_cannot_decode_is_retryable() {
        let error = InferenceError::CompletionFailed(CompletionError::ProviderResponse(
            ProviderResponseError::new(axum::http::StatusCode::OK, r#"{"choices":[]}"#),
        ));

        assert!(error.is_retryable());
        assert_eq!(error.retry_reason(), "invalid_provider_response");
    }

    #[test]
    fn malformed_provider_json_is_retryable() {
        let json_error =
            serde_json::from_str::<serde_json::Value>(r#"{"output":"cut off"#).unwrap_err();
        let error = InferenceError::CompletionFailed(CompletionError::JsonError(json_error));

        assert!(error.is_retryable());
        assert_eq!(error.retry_reason(), "invalid_provider_response");
    }

    #[test]
    fn a_provider_response_without_an_http_success_status_is_not_retryable() {
        let error = InferenceError::CompletionFailed(CompletionError::ProviderResponse(
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

impl From<InferenceError> for crate::core::error::AppError {
    fn from(err: InferenceError) -> Self {
        crate::core::error::AppError::Inference(Box::new(err))
    }
}
