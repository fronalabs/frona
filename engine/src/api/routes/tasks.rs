use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use crate::task::dto::{CreateTaskRequest, TaskResponse, UpdateTaskRequest};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use super::super::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/tasks", get(list_active_tasks).post(create_task))
        .route(
            "/api/tasks/{id}",
            axum::routing::put(update_task).delete(delete_task),
        )
}

async fn create_task(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<TaskResponse>, ApiError> {
    let response = state.task_service.create(&auth.user_id, req).await?;
    Ok(Json(response))
}

async fn list_active_tasks(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<TaskResponse>>, ApiError> {
    let tasks = state.task_service.list_active(&auth.user_id).await?;
    Ok(Json(tasks))
}

async fn update_task(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateTaskRequest>,
) -> Result<Json<TaskResponse>, ApiError> {
    let task = state.task_service.update(&auth.user_id, &id, req).await?;
    Ok(Json(task))
}

async fn delete_task(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(), ApiError> {
    state.task_service.delete(&auth.user_id, &id).await?;
    Ok(())
}
