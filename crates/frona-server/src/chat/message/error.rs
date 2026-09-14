use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::{core::error::AppError, inference::error::InferenceError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct MessageError {
    pub message: String,
    pub timestamp: DateTime<Utc>,
    pub details: MessageErrorDetails,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[serde(tag = "subsystem", content = "data", rename_all = "snake_case")]
#[surreal(
    crate = "surrealdb::types",
    tag = "subsystem",
    content = "data",
    rename_all = "snake_case"
)]
pub enum MessageErrorDetails {
    Inference(InferenceFailureDetails),
    ToolExecution(ToolFailureDetails),
    MessageProcessing(ProcessingFailureDetails),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "snake_case")]
#[surreal(crate = "surrealdb::types", rename_all = "snake_case")]
pub enum ErrorCategory {
    Authentication,
    Permission,
    ModelUnavailable,
    RateLimit,
    Timeout,
    Network,
    InvalidRequest,
    InvalidResponse,
    Configuration,
    Internal,
    Unknown,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct InferenceFailureDetails {
    pub category: ErrorCategory,
    pub retryable: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub retry_count: Option<u32>,
    pub fallback_count: Option<u32>,
    pub http_status: Option<u16>,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ToolFailureDetails {
    pub category: ErrorCategory,
    pub retryable: bool,
    pub tool_name: Option<String>,
    pub http_status: Option<u16>,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct ProcessingFailureDetails {
    pub category: ErrorCategory,
    pub retryable: bool,
    pub http_status: Option<u16>,
}

impl From<&AppError> for MessageError {
    fn from(error: &AppError) -> Self {
        let details = match error {
            AppError::Inference(error) => MessageErrorDetails::Inference(inference_details(error)),
            AppError::ToolExecution { tool_name, source } => {
                let (category, http_status) = processing_category(source);
                MessageErrorDetails::ToolExecution(ToolFailureDetails {
                    category,
                    retryable: source.is_retryable(),
                    tool_name: Some(tool_name.clone()),
                    http_status,
                })
            }
            AppError::Tool(_) => MessageErrorDetails::ToolExecution(ToolFailureDetails {
                category: ErrorCategory::Unknown,
                retryable: error.is_retryable(),
                tool_name: None,
                http_status: None,
            }),
            _ => {
                let (category, http_status) = processing_category(error);
                MessageErrorDetails::MessageProcessing(ProcessingFailureDetails {
                    category,
                    retryable: error.is_retryable(),
                    http_status,
                })
            }
        };
        Self {
            message: error.to_string(),
            timestamp: Utc::now(),
            details,
        }
    }
}

fn http_category(status: u16) -> ErrorCategory {
    match status {
        401 => ErrorCategory::Authentication,
        403 => ErrorCategory::Permission,
        408 | 504 => ErrorCategory::Timeout,
        429 => ErrorCategory::RateLimit,
        400..=499 => ErrorCategory::InvalidRequest,
        500..=599 => ErrorCategory::Internal,
        _ => ErrorCategory::Unknown,
    }
}

fn processing_category(error: &AppError) -> (ErrorCategory, Option<u16>) {
    let category = match error {
        AppError::Auth { .. } => ErrorCategory::Authentication,
        AppError::Forbidden(_) => ErrorCategory::Permission,
        AppError::NotFound(_) | AppError::Validation(_) | AppError::Conflict(_) => {
            ErrorCategory::InvalidRequest
        }
        AppError::Http { status, .. } => return (http_category(*status), Some(*status)),
        AppError::Database(_) | AppError::Internal(_) | AppError::Decryption(_) => {
            ErrorCategory::Internal
        }
        AppError::ToolExecution { source, .. } => return processing_category(source),
        _ => ErrorCategory::Unknown,
    };
    (category, None)
}

fn inference_details(error: &InferenceError) -> InferenceFailureDetails {
    if let InferenceError::ModelFailed {
        provider,
        model,
        retry_count,
        source,
    } = error
    {
        let mut details = inference_details(source);
        details.provider = Some(provider.clone());
        details.model = Some(model.clone());
        details.retry_count = Some(*retry_count);
        details.fallback_count = Some(0);
        return details;
    }
    if let InferenceError::AllFallbacksFailed(failures) = error
        && let Some(last) = failures.last()
    {
        let mut details = inference_details(last);
        details.retry_count = failures
            .iter()
            .map(|failure| inference_details(failure).retry_count)
            .sum();
        details.fallback_count = Some(failures.len().saturating_sub(1) as u32);
        // All candidates have been exhausted; callers must not restart the group automatically.
        details.retryable = false;
        return details;
    }
    let mut http_status = None;
    let category = match error {
        InferenceError::ProviderNotConfigured(_)
        | InferenceError::ModelGroupNotFound(_)
        | InferenceError::ConfigError(_) => ErrorCategory::Configuration,
        InferenceError::RateLimited { .. } => ErrorCategory::RateLimit,
        InferenceError::EmptyResponse => ErrorCategory::InvalidResponse,
        InferenceError::CompletionFailed(error) => {
            use rig_core::{completion::CompletionError, http_client::Error};
            match error {
                CompletionError::HttpError(
                    Error::InvalidStatusCode(status)
                    | Error::InvalidStatusCodeWithMessage(status, _),
                ) => {
                    http_status = Some(status.as_u16());
                    if status.as_u16() == 404 {
                        ErrorCategory::ModelUnavailable
                    } else {
                        http_category(status.as_u16())
                    }
                }
                CompletionError::HttpError(Error::Instance(_)) => ErrorCategory::Network,
                CompletionError::JsonError(_) => ErrorCategory::InvalidResponse,
                CompletionError::ProviderResponse(response) => {
                    http_status = response.status.map(|status| status.as_u16());
                    http_status
                        .filter(|status| *status >= 400)
                        .map(http_category)
                        .unwrap_or(ErrorCategory::InvalidResponse)
                }
                _ => ErrorCategory::Unknown,
            }
        }
        _ => ErrorCategory::Unknown,
    };
    InferenceFailureDetails {
        category,
        retryable: error.is_retryable(),
        provider: None,
        model: None,
        retry_count: None,
        fallback_count: None,
        http_status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{
            Handle,
            config::{ProviderModel, RetryConfig},
        },
        inference::{provider::ModelConfig, retry::retry_with_backoff},
    };
    use std::sync::atomic::{AtomicU32, Ordering};

    fn model(provider: &str, model: &str) -> ModelConfig {
        ModelConfig {
            catalog_provider: "openai".into(),
            provider_handle: Handle::try_new(provider).unwrap(),
            model_id: model.into(),
            provider: ProviderModel::Custom {
                name: "test".into(),
            },
            request_settings: Default::default(),
        }
    }

    #[tokio::test]
    async fn exhausted_retries_keep_typed_causes_and_actual_counts() {
        let retry = RetryConfig {
            max_retries: 2,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            ..Default::default()
        };
        let calls = AtomicU32::new(0);
        let primary =
            retry_with_backoff::<(), _, _>(&retry, &model("primary", "limited"), || async {
                calls.fetch_add(1, Ordering::Relaxed);
                Err(InferenceError::RateLimited {
                    retry_after_secs: 1,
                })
            })
            .await
            .err()
            .unwrap();
        let fallback = retry_with_backoff::<(), _, _>(
            &retry,
            &model("subscription", "gpt-5.3-codex"),
            || async {
                calls.fetch_add(1, Ordering::Relaxed);
                Err(InferenceError::CompletionFailed(
                    rig_core::completion::CompletionError::HttpError(
                        rig_core::http_client::Error::InvalidStatusCodeWithMessage(
                            axum::http::StatusCode::BAD_REQUEST,
                            "Model unavailable for this account".into(),
                        ),
                    ),
                ))
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        let started = Utc::now();
        let failure =
            MessageError::from(&AppError::from(InferenceError::AllFallbacksFailed(vec![
                primary, fallback,
            ])));
        assert!(failure.timestamp >= started && failure.timestamp <= Utc::now());
        let MessageErrorDetails::Inference(details) = &failure.details else {
            panic!("expected inference details")
        };
        assert_eq!(details.retry_count, Some(2));
        assert_eq!(details.fallback_count, Some(1));
        assert_eq!(details.http_status, Some(400));
        assert_eq!(details.provider.as_deref(), Some("subscription"));
        assert_eq!(details.model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(details.category, ErrorCategory::InvalidRequest);
        assert!(!details.retryable);
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["details"]["subsystem"], "inference");
        assert_eq!(
            serde_json::from_value::<MessageError>(json).unwrap(),
            failure
        );
    }

    #[test]
    fn tool_and_processing_errors_keep_their_own_details() {
        let error = AppError::ToolExecution {
            tool_name: "web_search".into(),
            source: Box::new(AppError::Http {
                status: 429,
                message: "Too many requests".into(),
            }),
        };
        let failure = MessageError::from(&error);
        let MessageErrorDetails::ToolExecution(details) = failure.details else {
            panic!("expected tool details")
        };
        assert_eq!(details.tool_name.as_deref(), Some("web_search"));
        assert_eq!(details.category, ErrorCategory::RateLimit);
        assert!(details.retryable);
        let failure = MessageError::from(&AppError::Validation("Missing message".into()));
        assert!(matches!(
            failure.details,
            MessageErrorDetails::MessageProcessing(ProcessingFailureDetails {
                category: ErrorCategory::InvalidRequest,
                ..
            })
        ));
    }
}
