use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::json;

use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/system/health", get(health_handler))
        .route("/healthz", get(health_handler))
        .route("/api/system/info", get(info_handler))
        .route("/api/system/version", get(version_handler))
        .route("/api/system/timezones", get(timezones_handler))
        .route("/api/system/restart", post(restart_handler))
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    if state.is_shutting_down() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"status": "draining"})),
        )
    } else {
        (StatusCode::OK, axum::Json(json!({"status": "ok"})))
    }
}

async fn info_handler(
    _auth: AuthUser,
    State(state): State<AppState>,
) -> axum::Json<serde_json::Value> {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_memory();
    let total_memory = sys
        .cgroup_limits()
        .map(|cg| cg.total_memory)
        .unwrap_or_else(|| sys.total_memory());
    let cpus = System::physical_core_count().unwrap_or(0);

    axum::Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "revision": option_env!("FRONA_BUILD_REVISION")
            .filter(|revision| !revision.is_empty() && *revision != "unknown"),
        "cpus": cpus,
        "total_memory_bytes": total_memory,
        "sandbox_driver": state.sandbox_factory.driver_id(),
        "server_timezone": state.config.server.timezone,
    }))
}

async fn version_handler(_auth: AuthUser) -> axum::Json<serde_json::Value> {
    axum::Json(json!({"version": env!("CARGO_PKG_VERSION")}))
}

async fn timezones_handler(_auth: AuthUser) -> axum::Json<Vec<String>> {
    axum::Json(list_iana_timezones())
}

fn list_iana_timezones() -> Vec<String> {
    let mut zones: Vec<String> = chrono_tz::TZ_VARIANTS
        .iter()
        .map(|tz| tz.name().to_string())
        .filter(|s| s.contains('/'))
        .collect();
    zones.sort();
    zones
}

async fn restart_handler(
    _auth: AuthUser,
    State(state): State<AppState>,
) -> axum::Json<serde_json::Value> {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        tracing::info!("Restart requested, draining in-flight work...");
        crate::core::shutdown::graceful_drain(&state).await;
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
