use std::time::Duration;

use async_trait::async_trait;
use tracing::info;

use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::core::supervisor::Supervisor;
use crate::notification::models::NotificationData;

use super::manager::ProcessStatus;
use super::models::AppStatus;

pub struct AppSupervisor {
    state: AppState,
}

impl AppSupervisor {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Supervisor for AppSupervisor {
    fn label(&self) -> &'static str {
        "app"
    }

    async fn find_running(&self) -> Result<Vec<String>, AppError> {
        let apps = self.state.app_service.find_running().await?;
        Ok(apps
            .into_iter()
            .filter(|a| a.kind != "static")
            .map(|a| a.id)
            .collect())
    }

    async fn start(&self, id: &str) -> Result<(), AppError> {
        if self.state.app_service.manager().has_process(id).await {
            match self
                .state
                .app_service
                .manager()
                .try_restart_crashed(id, self.state.app_service.max_restart_attempts())
                .await?
            {
                Some((port, pid)) => {
                    let _ = self
                        .state
                        .app_service
                        .update_status(id, AppStatus::Running, Some(port), Some(pid))
                        .await;
                }
                None => return Err(AppError::Tool("restart returned None".into())),
            }
            return Ok(());
        }

        let app = self
            .state
            .app_service
            .get(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("app {id}")))?;
        let Some(ref command) = app.command else {
            return Ok(());
        };
        let manifest: super::models::AppManifest = serde_json::from_value(app.manifest.clone())
            .map_err(|e| AppError::Tool(format!("bad manifest: {e}")))?;
        let (port, pid) = self
            .state
            .app_service
            .manager()
            .start_app(
                id,
                &app.agent_id,
                &app.user_id,
                command,
                &manifest,
                Vec::new(),
            )
            .await?;
        let _ = self
            .state
            .app_service
            .update_status(id, AppStatus::Running, Some(port), Some(pid))
            .await;
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), AppError> {
        self.state.app_service.manager().stop_app(id).await
    }

    async fn find_dead(&self) -> Result<Vec<String>, AppError> {
        let ids = self.state.app_service.manager().get_managed_app_ids().await;
        let mut dead = Vec::new();
        for id in ids {
            if let ProcessStatus::Dead(_) =
                self.state.app_service.manager().check_process(&id).await
            {
                dead.push(id);
            }
        }
        Ok(dead)
    }

    async fn restart_count(&self, id: &str) -> u32 {
        self.state.app_service.manager().restart_count_for(id).await
    }

    async fn mark_failed(&self, id: &str, _reason: &str) -> Result<(), AppError> {
        let _ = self
            .state
            .app_service
            .update_status(id, AppStatus::Failed, None, None)
            .await;
        self.state.app_service.manager().remove_process(id).await;
        Ok(())
    }

    async fn record_access(&self, id: &str) {
        self.state.app_service.manager().record_access(id).await;
    }

    async fn find_idle(&self, idle_threshold: Duration) -> Result<Vec<String>, AppError> {
        let running_apps = self.state.app_service.find_running().await?;
        let now = chrono::Utc::now();
        let threshold_secs = idle_threshold.as_secs();
        let mut idle = Vec::new();

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
            let last = self
                .state
                .app_service
                .manager()
                .get_last_accessed(&app.id)
                .await
                .or(app.last_accessed_at)
                .unwrap_or(app.updated_at);
            let idle_secs = (now - last).num_seconds() as u64;
            if idle_secs >= threshold_secs {
                idle.push(app.id);
            }
        }
        Ok(idle)
    }

    async fn mark_hibernated(&self, id: &str) -> Result<(), AppError> {
        let _ = self
            .state
            .app_service
            .update_status(id, AppStatus::Hibernated, None, None)
            .await;
        Ok(())
    }

    async fn owner_of(&self, id: &str) -> Result<String, AppError> {
        let app = self
            .state
            .app_service
            .get(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("app {id}")))?;
        Ok(app.user_id)
    }

    async fn display_name(&self, id: &str) -> String {
        self.state
            .app_service
            .get(id)
            .await
            .ok()
            .flatten()
            .map(|a| a.name)
            .unwrap_or_else(|| id.to_string())
    }

    async fn attempt_auto_fix(&self, id: &str) -> bool {
        let Ok(Some(app)) = self.state.app_service.get(id).await else {
            return false;
        };
        if app.crash_fix_attempts > 0 {
            return false;
        }
        if let Some(mut app) = self.state.app_service.get(&app.id).await.ok().flatten() {
            app.crash_fix_attempts += 1;
            let _ = self
                .state
                .app_service
                .update_crash_fix_attempts(&app.id, app.crash_fix_attempts)
                .await;
        }
        let app_handle = app.handle.to_string();
        let Some(crash_msg) = self.state.prompts.read_with_vars(
            "APP_CRASH.md",
            &[("app_name", &app.name), ("app_handle", &app_handle)],
        ) else {
            return false;
        };
        let _ = self
            .state
            .chat_service
            .save_system_message(&app.user_id, None, &app.chat_id, crash_msg)
            .await;
        let state = self.state.clone();
        let user_id = app.user_id.clone();
        let chat_id = app.chat_id.clone();
        let agent_id = app.agent_id.clone();
        let app_name = app.name.clone();
        tokio::spawn(async move {
            let message_id = match state
                .chat_service
                .find_executing_message_for_chat(&chat_id)
                .await
            {
                Ok(Some(msg)) => msg.id,
                Ok(None) => {
                    info!(chat_id = %chat_id, "Creating agent message for crash fix");
                    match state
                        .chat_service
                        .create_executing_agent_message(&chat_id, &agent_id)
                        .await
                    {
                        Ok(msg) => msg.id,
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to create agent message for crash fix");
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to find executing message");
                    return;
                }
            };
            let execution = crate::core::execution::NewExecution {
                title: format!("Fixing {app_name}"),
                kind: crate::core::execution::ExecutionKind::App,
                action: Some("Repairing crashed app".to_string()),
                source: Some(crate::core::execution::ExecutionSource {
                    kind: crate::core::execution::ExecutionSourceKind::Chat,
                    id: Some(chat_id.clone()),
                }),
                related_chat_ids: vec![chat_id.clone()],
                can_cancel: true,
            };
            if let Err(e) = state
                .harness
                .resume_with_execution(&user_id, &chat_id, &message_id, execution)
                .await
            {
                tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume app chat for crash fix");
            }
        });
        true
    }

    async fn notification_data(&self, id: &str, action: &str) -> NotificationData {
        // Resolve UUID → handle for the notification deep-link.
        let handle = self
            .state
            .app_service
            .get(id)
            .await
            .ok()
            .flatten()
            .map(|a| a.handle.to_string())
            .unwrap_or_else(|| id.to_string());
        NotificationData::App {
            app_handle: handle,
            action: action.to_string(),
        }
    }
}
