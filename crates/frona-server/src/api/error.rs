use crate::core::error::{AppError, AuthErrorCode};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Byte-identical 404 for routes that must not leak whether a resource
/// exists. Uses a plain-text body (not `ApiError`'s `{"error": "..."}`) so
/// different failure causes can't be told apart by length.
pub fn anonymous_not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

pub struct ApiError(pub AppError);

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self.0 {
            AppError::Auth { message, code } => {
                let status = match code {
                    AuthErrorCode::AccountDeactivated => StatusCode::FORBIDDEN,
                    _ => StatusCode::UNAUTHORIZED,
                };
                (status, message.clone())
            }
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Validation(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Database(msg) => {
                tracing::error!("Database error: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".into(),
                )
            }
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::Internal(msg) => {
                tracing::error!("Internal error: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".into(),
                )
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
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
            AppError::ToolExecution { .. } => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string())
            }
            AppError::Decryption(msg) => {
                tracing::error!("Decryption error: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".into(),
                )
            }
            AppError::Http { status, message } => (
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
                message.clone(),
            ),
        };

        let mut message_error = crate::chat::message::error::MessageError::from(&self.0);
        message_error.message = message.clone();
        (
            status,
            Json(json!({ "error": message, "message_error": message_error })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn structured_api_errors_preserve_public_message_and_timestamp() {
        let response =
            ApiError(AppError::Database("private database detail".into())).into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Internal server error");
        assert_eq!(json["message_error"]["message"], json["error"]);
        assert_eq!(
            json["message_error"]["details"]["subsystem"],
            "message_processing"
        );
        assert_eq!(
            json["message_error"]["details"]["data"]["category"],
            "internal"
        );
        assert!(
            chrono::DateTime::parse_from_rfc3339(
                json["message_error"]["timestamp"].as_str().unwrap()
            )
            .is_ok()
        );
        assert!(
            !String::from_utf8(body.to_vec())
                .unwrap()
                .contains("private database detail")
        );
    }
}
