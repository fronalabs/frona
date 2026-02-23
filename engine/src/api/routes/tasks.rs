use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use crate::agent::task::models::{CreateTaskRequest, TaskResponse, UpdateTaskRequest};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/tasks", get(list_active_tasks).post(create_task))
        .route(
            "/api/tasks/{id}",
            get(get_task).put(update_task).delete(delete_task),
        )
        .route("/api/tasks/{id}/cancel", axum::routing::post(cancel_task))
}

async fn get_task(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskResponse>, ApiError> {
    let task = state
        .task_service
        .find_by_id(&id)
        .await?
        .ok_or_else(|| crate::core::error::AppError::NotFound("Task not found".into()))?;

    if task.user_id != auth.user_id {
        return Err(crate::core::error::AppError::Forbidden("Not your task".into()).into());
    }

    Ok(Json(task.into()))
}

async fn create_task(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<TaskResponse>, ApiError> {
    let response = state.task_service.create(&auth.user_id, req).await?;
    state.broadcast_service.broadcast_task_update(
        &auth.user_id,
        &response.id,
        "pending",
        &response.title,
        response.chat_id.as_deref(),
        None,
        None,
    );
    Ok(Json(response))
}

async fn list_active_tasks(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<TaskResponse>>, ApiError> {
    let tasks = state.task_service.list_all(&auth.user_id).await?;
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

async fn cancel_task(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskResponse>, ApiError> {
    let task = state.task_service.cancel(&auth.user_id, &id).await?;

    if let Some(executor) = state.task_executor() {
        executor.cancel_task(&id).await;
    }

    Ok(Json(task.into()))
}
