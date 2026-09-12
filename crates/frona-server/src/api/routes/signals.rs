use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::agent::signal::{Annotation, CandidateEvent};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/signals/evaluate", post(evaluate_signal))
}

#[derive(Debug, Deserialize)]
pub struct EvaluateSignalRequest {
    #[serde(default, alias = "tags")]
    pub categories: Vec<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub connector_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub contact_id: Option<String>,
    #[serde(default)]
    pub sender: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct EvaluateSignalResponse {
    pub fired_watches: Vec<String>,
}

const MAX_CATEGORIES: usize = 32;
const MAX_CONTENT_BYTES: usize = 64 * 1024;
const HTTP_ANNOTATOR_ID: &str = "http";

async fn evaluate_signal(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<EvaluateSignalRequest>,
) -> Result<Json<EvaluateSignalResponse>, ApiError> {
    if req.categories.is_empty() {
        return Err(
            AppError::Validation("categories must contain at least one entry".into()).into(),
        );
    }
    if req.categories.len() > MAX_CATEGORIES {
        return Err(AppError::Validation(format!(
            "categories must contain at most {MAX_CATEGORIES} entries"
        ))
        .into());
    }
    if req.content.len() > MAX_CONTENT_BYTES {
        return Err(AppError::Validation(format!(
            "content must be at most {MAX_CONTENT_BYTES} bytes"
        ))
        .into());
    }

    let mut annotations: Vec<Annotation> = req
        .categories
        .into_iter()
        .map(|c| Annotation::category(HTTP_ANNOTATOR_ID, c))
        .collect();
    if let Some(s) = req.summary {
        annotations.push(Annotation::summary(HTTP_ANNOTATOR_ID, s));
    }

    let candidate = CandidateEvent {
        channel: None,
        chat: None,
        message: None,
        contact: None,
        sender: req.sender,
        annotations,
        content: req.content,
    };

    let signal_service = state.signal_service();
    let fired_watches = signal_service.evaluate(&auth.user_id, candidate).await?;
    Ok(Json(EvaluateSignalResponse { fired_watches }))
}
