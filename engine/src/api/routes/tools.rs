use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::super::state::AppState;

#[derive(Serialize, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/tools", get(list_tools))
}

async fn list_tools(State(state): State<AppState>) -> Json<Vec<ToolInfo>> {
    let mut tools: Vec<ToolInfo> = state
        .cli_tools_config
        .iter()
        .map(|t| ToolInfo {
            name: t.name.clone(),
            description: t.description.clone(),
        })
        .collect();

    tools.push(ToolInfo {
        name: "browser".to_string(),
        description: "Web browser automation".to_string(),
    });

    tools.push(ToolInfo {
        name: "web_fetch".to_string(),
        description: "Fetch a web page and return its content as markdown".to_string(),
    });

    tools.push(ToolInfo {
        name: "web_search".to_string(),
        description: "Search the web and return structured results".to_string(),
    });

    Json(tools)
}
