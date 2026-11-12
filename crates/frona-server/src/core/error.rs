use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthErrorCode {
    InvalidCredentials,
    EmailNotVerified,
    CsrfFailed,
    TokenInvalid,
    TokenFailed,
    SsoDisabled,
    ServerError,
    AccountDeactivated,
}

impl AuthErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidCredentials => "invalid_credentials",
            Self::EmailNotVerified => "email_not_verified",
            Self::CsrfFailed => "csrf_failed",
            Self::TokenInvalid => "token_invalid",
            Self::TokenFailed => "token_failed",
            Self::SsoDisabled => "sso_disabled",
            Self::ServerError => "server_error",
            Self::AccountDeactivated => "account_deactivated",
        }
    }
}

impl std::fmt::Display for AuthErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Authentication failed: {message}")]
    Auth {
        message: String,
        code: AuthErrorCode,
    },

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Decryption error: {0}")]
    Decryption(String),

    #[error("Inference error: {0}")]
    Inference(#[source] Box<frona_harness::inference::error::InferenceError>),

    #[error("Browser error: {0}")]
    Browser(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("Tool '{tool_name}' failed: {source}")]
    ToolExecution {
        tool_name: String,
        #[source]
        source: Box<AppError>,
    },

    #[error("HTTP error {status}: {message}")]
    Http { status: u16, message: String },
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Tool(format!("json: {e}"))
    }
}

impl AppError {
    pub fn is_retryable(&self) -> bool {
        match self {
            AppError::Inference(error) => error.is_retryable(),
            AppError::ToolExecution { source, .. } => source.is_retryable(),
            AppError::Http { status, .. } => matches!(status, 429 | 500 | 502 | 503 | 504),
            AppError::Tool(msg) => {
                let lower = msg.to_lowercase();
                lower.contains("timeout") || lower.contains("connection")
            }
            _ => false,
        }
    }
}

impl From<frona_harness::inference::InferenceError> for crate::core::error::AppError {
    fn from(err: frona_harness::inference::InferenceError) -> Self {
        crate::core::error::AppError::Inference(Box::new(err))
    }
}

impl From<frona_harness::tools::files::FileError> for AppError {
    fn from(error: frona_harness::tools::files::FileError) -> Self {
        use frona_harness::tools::files::FileError;
        match error {
            FileError::NotFound(message) => Self::NotFound(message),
            FileError::Validation(message) => Self::Validation(message),
            FileError::Io(message) => Self::Internal(message),
        }
    }
}

impl From<frona_text::prompt::PromptError> for AppError {
    fn from(error: frona_text::prompt::PromptError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<frona_pkm::Error> for AppError {
    fn from(error: frona_pkm::Error) -> Self {
        match error {
            frona_pkm::Error::NotFound(message) => Self::NotFound(message),
            frona_pkm::Error::BadRequest(message) | frona_pkm::Error::Validation(message) => {
                Self::Validation(message)
            }
            frona_pkm::Error::Conflict(message) => Self::Conflict(message),
            frona_pkm::Error::Internal(message) => Self::Internal(message),
            frona_pkm::Error::Cancelled(message) => {
                frona_harness::inference::InferenceError::Cancelled(message).into()
            }
            frona_pkm::Error::Io(error) => Self::Internal(error.to_string()),
        }
    }
}
