use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Browser error: {0}")]
    Browser(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("HTTP error {status}: {message}")]
    Http { status: u16, message: String },
}

impl AppError {
    pub fn is_retryable(&self) -> bool {
        match self {
            AppError::Http { status, .. } => matches!(status, 429 | 500 | 502 | 503 | 504),
            AppError::Tool(msg) => {
                let lower = msg.to_lowercase();
                lower.contains("timeout") || lower.contains("connection")
            }
            _ => false,
        }
    }
}
