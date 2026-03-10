use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use crate::core::error::AppError;
use serde_json::json;

pub struct ApiError(pub AppError);

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self.0 {
            AppError::Auth(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Validation(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Database(msg) => {
                tracing::error!("Database error: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into())
            }
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Internal(msg) => {
                tracing::error!("Internal error: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into())
            }
            AppError::Inference(msg) => {
                tracing::error!("Inference error: {msg}");
                (StatusCode::BAD_GATEWAY, "Inference service error".into())
            }
            AppError::Browser(msg) => {
                tracing::error!("Browser error: {msg}");
                (StatusCode::BAD_GATEWAY, "Browser service error".into())
            }
            AppError::Tool(msg) => {
                tracing::error!("Tool error: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Tool execution error".into())
            }
            AppError::Decryption(msg) => {
                tracing::error!("Decryption error: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into())
            }
            AppError::Http { status, message } => {
                (StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY), message.clone())
            }
        };

        (status, Json(json!({ "error": message }))).into_response()
    }
}
