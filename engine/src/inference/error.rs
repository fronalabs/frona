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
    CompletionFailed(#[from] rig::completion::CompletionError),

    #[error("Invalid model reference: {0}")]
    InvalidModelRef(String),

    #[error("All fallbacks failed: {}", format_fallback_errors(.0))]
    AllFallbacksFailed(Vec<(String, String)>),

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

impl InferenceError {
    pub fn is_retryable(&self) -> bool {
        match self {
            InferenceError::RateLimited { .. } | InferenceError::EmptyResponse => true,
            InferenceError::Cancelled(_) => false,
            InferenceError::CompletionFailed(rig::completion::CompletionError::HttpError(http_err)) => {
                    use rig::http_client::Error;
                    match http_err {
                        Error::InvalidStatusCode(s)
                        | Error::InvalidStatusCodeWithMessage(s, _) => {
                            let code = s.as_u16();
                            code == 429
                                || code == 500
                                || code == 502
                                || code == 503
                                || code == 504
                        }
                        Error::Instance(_) => true,
                        _ => false,
                    }
            }
            InferenceError::CompletionFailed(rig::completion::CompletionError::ProviderError(msg)) => {
                provider_error_contains_status(msg, &[429, 500, 502, 503, 504])
            }
            InferenceError::CompletionFailed(_) => false,
            InferenceError::InferenceFailed(msg) | InferenceError::StreamingFailed(msg) => {
                let lower = msg.to_lowercase();
                lower.contains("429")
                    || lower.contains("timeout")
                    || lower.contains("overloaded")
            }
            _ => false,
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        match self {
            InferenceError::RateLimited { .. } => true,
            InferenceError::CompletionFailed(rig::completion::CompletionError::HttpError(http_err)) => {
                use rig::http_client::Error;
                matches!(
                    http_err,
                    Error::InvalidStatusCode(s) | Error::InvalidStatusCodeWithMessage(s, _)
                    if s.as_u16() == 429
                )
            }
            InferenceError::CompletionFailed(rig::completion::CompletionError::ProviderError(msg)) => {
                provider_error_contains_status(msg, &[429])
            }
            _ => false,
        }
    }
}

fn format_fallback_errors(errors: &[(String, String)]) -> String {
    errors
        .iter()
        .map(|(model, err)| format!("{model}: {err}"))
        .collect::<Vec<_>>()
        .join("; ")
}

impl From<InferenceError> for crate::core::error::AppError {
    fn from(err: InferenceError) -> Self {
        crate::core::error::AppError::Inference(err.to_string())
    }
}
