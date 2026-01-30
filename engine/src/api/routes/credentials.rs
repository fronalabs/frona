use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};

use crate::credential::dto::{CreateCredentialRequest, CredentialResponse};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use super::super::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/credentials",
            get(list_credentials).post(create_credential),
        )
        .route("/api/credentials/{id}", axum::routing::delete(delete_credential))
}

async fn create_credential(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateCredentialRequest>,
) -> Result<Json<CredentialResponse>, ApiError> {
    let response = state
        .credential_service
        .create(&auth.user_id, req)
        .await?;
    Ok(Json(response))
}

async fn list_credentials(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<CredentialResponse>>, ApiError> {
    let credentials = state.credential_service.list(&auth.user_id).await?;
    Ok(Json(credentials))
}

async fn delete_credential(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(), ApiError> {
    state
        .credential_service
        .delete(&auth.user_id, &id)
        .await?;
    Ok(())
}
