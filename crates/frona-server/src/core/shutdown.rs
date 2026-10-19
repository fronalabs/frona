use std::time::Duration;

use tracing::info;

use super::state::AppState;

/// Cancel shutdown token, drain active sessions (with timeout), and kill browser sessions.
pub async fn graceful_drain(state: &AppState) {
    state.shutdown_token.cancel();

    let timeout = Duration::from_secs(state.config.server.shutdown_timeout_secs);
    let drain = async {
        loop {
            let count =
                state.active_sessions.count().await + state.task_executor.active_count().await;
            if count == 0 {
                info!("All in-flight work drained");
                break;
            }
            info!(
                active_executions = count,
                "Waiting for in-flight work to complete..."
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };

    match tokio::time::timeout(timeout, drain).await {
        Ok(()) => info!("Graceful shutdown complete"),
        Err(_) => tracing::warn!(
            timeout_secs = state.config.server.shutdown_timeout_secs,
            "Shutdown timeout reached, forcing exit"
        ),
    }

    state.browser_session_manager.kill_all_sessions().await;
}
