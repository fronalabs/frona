use axum::Router;
use axum::routing::post;
use serde_json::json;

use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/system/restart", post(restart_handler))
}

async fn restart_handler(_auth: AuthUser) -> axum::Json<serde_json::Value> {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        re_exec_self();
    });
    axum::Json(json!({"status": "restarting"}))
}

fn re_exec_self() -> ! {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().expect("failed to get current executable path");
    let args: Vec<String> = std::env::args().skip(1).collect();

    let err = std::process::Command::new(&exe).args(&args).exec();
    panic!("exec failed: {err}");
}
