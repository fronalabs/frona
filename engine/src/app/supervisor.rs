use tracing::info;

use crate::core::state::AppState;

use super::models::AppStatus;

pub async fn restore_and_supervise_apps(
    state: AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    let apps = state.app_service.find_running().await?;
    info!(count = apps.len(), "Found apps to restore");
    for app in &apps {
        info!(
            app_id = %app.id,
            name = %app.name,
            kind = %app.kind,
            status = ?app.status,
            agent_id = %app.agent_id,
            "Restoring app"
        );
        if app.kind == "static" {
            info!(app_id = %app.id, "Skipping static app (no process needed)");
            continue;
        }
        if let Some(ref command) = app.command {
            let manifest: super::models::AppManifest =
                serde_json::from_value(app.manifest.clone())?;
            match state
                .app_service
                .manager()
                .start_app(&app.id, &app.agent_id, command, &manifest, Vec::new())
                .await
            {
                Ok((port, pid)) => {
                    let _ = state
                        .app_service
                        .update_status(&app.id, AppStatus::Running, Some(port), Some(pid))
                        .await;
                    info!(app_id = %app.id, port, "Restored app");
                }
                Err(e) => {
                    tracing::warn!(app_id = %app.id, error = %e, "Failed to restore app");
                    let _ = state
                        .app_service
                        .update_status(&app.id, AppStatus::Failed, None, None)
                        .await;
                }
            }
        }
    }

    info!(count = apps.len(), "App restoration complete, starting supervision");

    let max_restarts = state.app_service.max_restart_attempts();
    let hibernate_secs = state.app_service.hibernate_after_secs();

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        let access_times = state.app_service.manager().flush_access_times().await;
        for (app_id, at) in &access_times {
            let _ = state.app_service.update_last_accessed(app_id, *at).await;
        }

        let app_ids = state.app_service.manager().get_managed_app_ids().await;
        for app_id in &app_ids {
            if !state.app_service.manager().is_process_alive(app_id).await {
                tracing::warn!(app_id = %app_id, "App process died, attempting restart");
                match state
                    .app_service
                    .manager()
                    .try_restart_crashed(app_id, max_restarts)
                    .await
                {
                    Ok(Some((port, pid))) => {
                        let _ = state
                            .app_service
                            .update_status(app_id, AppStatus::Running, Some(port), Some(pid))
                            .await;
                        tracing::info!(app_id = %app_id, "App restarted after crash");
                    }
                    Ok(None) => {
                        let _ = state
                            .app_service
                            .update_status(app_id, AppStatus::Failed, None, None)
                            .await;
                        tracing::warn!(app_id = %app_id, "App exceeded max restarts");
                    }
                    Err(e) => {
                        tracing::error!(app_id = %app_id, error = %e, "Failed to restart app");
                    }
                }
            }
        }

        if hibernate_secs > 0 {
            let now = chrono::Utc::now();
            if let Ok(running_apps) = state.app_service.find_running().await {
                for app in running_apps {
                    if app.kind == "static" || app.status != AppStatus::Running {
                        continue;
                    }
                    let manifest: Result<super::models::AppManifest, _> =
                        serde_json::from_value(app.manifest.clone());
                    if let Ok(m) = manifest
                        && !m.effective_hibernate()
                    {
                        continue;
                    }

                    let last = state
                        .app_service
                        .manager()
                        .get_last_accessed(&app.id)
                        .await
                        .or(app.last_accessed_at)
                        .unwrap_or(app.updated_at);

                    let idle = (now - last).num_seconds() as u64;
                    if idle >= hibernate_secs {
                        tracing::info!(app_id = %app.id, idle_secs = idle, "Hibernating idle app");
                        let _ = state.app_service.manager().stop_app(&app.id).await;
                        let _ = state
                            .app_service
                            .update_status(&app.id, AppStatus::Hibernated, None, None)
                            .await;
                    }
                }
            }
        }
    }
}
