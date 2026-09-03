use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::api::middleware::auth::AuthUser;
use crate::core::execution::ActivitySnapshot;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/activity", get(snapshot))
}

async fn snapshot(auth: AuthUser, State(state): State<AppState>) -> Json<ActivitySnapshot> {
    Json(state.execution_registry.snapshot(&auth.user_id))
}
