use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use crate::agent::dto::{AgentResponse, CreateAgentRequest, UpdateAgentRequest};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use super::super::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/agents", get(list_agents).post(create_agent))
        .route(
            "/api/agents/{id}",
            get(get_agent).put(update_agent).delete(delete_agent),
        )
}

async fn create_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<Json<AgentResponse>, ApiError> {
    let response = state.agent_service.create(&auth.user_id, req).await?;
    Ok(Json(response))
}

async fn list_agents(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentResponse>>, ApiError> {
    let agents = state.agent_service.list(&auth.user_id).await?;
    Ok(Json(agents))
}

async fn get_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentResponse>, ApiError> {
    let agent = state.agent_service.get(&auth.user_id, &id).await?;
    Ok(Json(agent))
}

async fn update_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAgentRequest>,
) -> Result<Json<AgentResponse>, ApiError> {
    let agent = state.agent_service.update(&auth.user_id, &id, req).await?;
    Ok(Json(agent))
}

async fn delete_agent(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(), ApiError> {
    state.agent_service.delete(&auth.user_id, &id).await?;
    Ok(())
}
