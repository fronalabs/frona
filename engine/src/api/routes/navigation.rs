use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::chat::dto::ChatResponse;
use crate::space::dto::SpaceResponse;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use super::super::state::AppState;

#[derive(Debug, Serialize)]
pub struct SpaceWithChats {
    #[serde(flatten)]
    pub space: SpaceResponse,
    pub chats: Vec<ChatResponse>,
}

#[derive(Debug, Serialize)]
pub struct NavigationResponse {
    pub spaces: Vec<SpaceWithChats>,
    pub standalone_chats: Vec<ChatResponse>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/navigation", get(get_navigation))
}

async fn get_navigation(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<NavigationResponse>, ApiError> {
    let spaces = state.space_service.list(&auth.user_id).await?;
    let standalone_chats = state
        .chat_service
        .find_standalone_chats_by_user(&auth.user_id)
        .await?;

    let mut space_with_chats = Vec::new();
    for space in spaces {
        let chats = state
            .chat_service
            .find_chats_by_space_id(&space.id)
            .await?;
        space_with_chats.push(SpaceWithChats {
            space,
            chats: chats.into_iter().map(Into::into).collect(),
        });
    }

    Ok(Json(NavigationResponse {
        spaces: space_with_chats,
        standalone_chats: standalone_chats.into_iter().map(Into::into).collect(),
    }))
}
