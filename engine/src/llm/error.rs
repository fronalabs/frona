use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
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

    #[error("Config error: {0}")]
    ConfigError(String),
}

impl LlmError {
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::RateLimited { .. } => true,
            LlmError::CompletionFailed(err) => match err {
                rig::completion::CompletionError::HttpError(http_err) => {
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
                _ => false,
            },
            LlmError::InferenceFailed(msg) | LlmError::StreamingFailed(msg) => {
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
            LlmError::RateLimited { .. } => true,
            LlmError::CompletionFailed(rig::completion::CompletionError::HttpError(http_err)) => {
                use rig::http_client::Error;
                matches!(
                    http_err,
                    Error::InvalidStatusCode(s) | Error::InvalidStatusCodeWithMessage(s, _)
                    if s.as_u16() == 429
                )
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

impl From<LlmError> for crate::error::AppError {
    fn from(err: LlmError) -> Self {
        crate::error::AppError::Llm(err.to_string())
    }
}
