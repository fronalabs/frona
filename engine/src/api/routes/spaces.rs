use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use crate::space::dto::{CreateSpaceRequest, SpaceResponse, UpdateSpaceRequest};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use super::super::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/spaces", get(list_spaces).post(create_space))
        .route(
            "/api/spaces/{id}",
            axum::routing::put(update_space).delete(delete_space),
        )
}

async fn create_space(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateSpaceRequest>,
) -> Result<Json<SpaceResponse>, ApiError> {
    let response = state.space_service.create(&auth.user_id, req).await?;
    Ok(Json(response))
}

async fn list_spaces(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<SpaceResponse>>, ApiError> {
    let spaces = state.space_service.list(&auth.user_id).await?;
    Ok(Json(spaces))
}

async fn update_space(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateSpaceRequest>,
) -> Result<Json<SpaceResponse>, ApiError> {
    let space = state.space_service.update(&auth.user_id, &id, req).await?;
    Ok(Json(space))
}

async fn delete_space(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(), ApiError> {
    state.space_service.delete(&auth.user_id, &id).await?;
    Ok(())
}
