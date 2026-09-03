use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::agent::models::Agent;
use crate::agent::task::models::TaskKind;
use crate::chat::models::CreateChatRequest;
use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::inference::InferenceResponse;
use crate::inference::conversation::HeartbeatConversationBuilder;
use crate::tool::task::next_cron_occurrence;

pub struct Scheduler {
    pub(crate) app_state: AppState,
}

macro_rules! spawn_periodic {
    ($scheduler:expr, $interval:expr, $name:expr, $method:ident, $shutdown:expr) => {{
        let s = $scheduler.clone();
        let shutdown = $shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep($interval) => {
                        if let Err(e) = s.$method().await {
                            tracing::warn!(error = %e, task = $name, "Scheduled task failed");
                        }
                    }
                    () = shutdown.cancelled() => {
                        tracing::info!(task = $name, "Scheduler loop stopping for shutdown");
                        break;
                    }
                }
            }
        });
    }};
}

impl Scheduler {
    pub fn new(app_state: AppState) -> Self {
        Self { app_state }
    }

    /// Spawn a periodic task that runs `task` every `interval` until shutdown.
    /// Generic - any component (e.g. a memory service) registers its own jobs,
    /// keeping the scheduler domain-agnostic.
    pub fn register_periodic<F, Fut>(&self, interval: Duration, name: &'static str, task: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), AppError>> + Send,
    {
        let shutdown = self.app_state.shutdown_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {
                        if let Err(e) = task().await {
                            tracing::warn!(error = %e, task = name, "Scheduled task failed");
                        }
                    }
                    () = shutdown.cancelled() => {
                        tracing::info!(task = name, "Scheduler loop stopping for shutdown");
                        break;
                    }
                }
            }
        });
    }

    pub fn start(self: Arc<Self>) {
        let cfg = &self.app_state.config;
        let poll = Duration::from_secs(cfg.scheduler.poll_secs);
        // Fixed cadence, like `model_metadata_refresh` below - token cleanup isn't a
        // tuning knob (it previously piggybacked on the memory `space_compaction_secs`).
        let token_cleanup = Duration::from_secs(3600);
        let model_metadata_refresh = Duration::from_secs(86400);

        let shutdown = self.app_state.shutdown_token.clone();
        spawn_periodic!(self, poll, "poll_tasks", run_poll_tasks, shutdown);
        spawn_periodic!(
            self,
            token_cleanup,
            "token_cleanup",
            run_token_cleanup,
            shutdown
        );
        spawn_periodic!(
            self,
            model_metadata_refresh,
            "model_metadata_refresh",
            run_model_metadata_refresh,
            shutdown
        );
        let share_cleanup = Duration::from_secs(cfg.share.cleanup_interval_secs);
        spawn_periodic!(
            self,
            share_cleanup,
            "share_cleanup",
            run_share_cleanup,
            shutdown
        );

        // Memory maintenance is owned by the active service (it registers its own
        // periodic jobs) - keeps the scheduler memory-agnostic and works for any impl.
        self.app_state
            .harness
            .memory_service
            .register_maintenance(&self);

        // Skip the startup refresh if the cached catalog is younger than the
        // periodic interval - restart shouldn't trigger another ~1.5 MB fetch.
        // Missing/unreadable cache always falls through to a fresh fetch.
        let cache_dir =
            std::path::Path::new(&self.app_state.config.storage.cache_dir).to_path_buf();
        let fresh = crate::inference::metadata::loader::cache_age(&cache_dir)
            .is_some_and(|age| age < model_metadata_refresh);
        if fresh {
            tracing::debug!("Skipping startup model-metadata refresh; cache is fresh");
        } else {
            let s = self.clone();
            tokio::spawn(async move {
                if let Err(e) = s.run_model_metadata_refresh().await {
                    tracing::warn!(error = %e, "Initial model-metadata refresh failed; keeping loaded catalog");
                }
            });
        }
    }

    /// Failures keep the previous table loaded.
    async fn run_model_metadata_refresh(&self) -> Result<(), AppError> {
        let raw = crate::inference::metadata::loader::fetch_metadata().await?;
        let parsed = crate::inference::metadata::loader::parse(&raw)?;
        let entries = parsed.entries.len() as f64;
        let version = parsed.version.clone();
        tracing::info!(
            version = %version,
            entries = entries as usize,
            "Refreshed model metadata catalog"
        );
        // Persist to disk so the next boot has a head start if the network is
        // unavailable. Failure is non-fatal - the in-memory swap below still
        // wins for this process.
        let cache_dir = std::path::Path::new(&self.app_state.config.storage.cache_dir);
        if let Err(e) = crate::inference::metadata::loader::save_cache(cache_dir, &raw) {
            tracing::warn!(error = %e, "Failed to persist model metadata cache");
        }
        // Swap the catalog directly - `model_catalog` is shared with
        // `usage_service` via internal `ArcSwap`, so all readers see the
        // new snapshot on next `current()`.
        self.app_state.model_catalog.swap(parsed);
        metrics::gauge!(
            crate::inference::usage::service::MODEL_METADATA_ENTRIES,
            "version" => version,
        )
        .set(entries);
        metrics::gauge!(crate::inference::usage::service::MODEL_METADATA_REFRESH_AGE_SECONDS)
            .set(0.0);
        Ok(())
    }

    async fn run_poll_tasks(&self) -> Result<(), AppError> {
        self.run_cron_tasks().await?;
        self.run_deferred_tasks().await?;
        self.run_signal_timeouts().await?;
        self.run_delivery_retries().await?;
        self.run_pair_expiry_sweep().await?;
        self.run_heartbeats().await
    }

    async fn run_pair_expiry_sweep(&self) -> Result<(), AppError> {
        if self.app_state.is_shutting_down() {
            return Ok(());
        }
        self.app_state
            .channel_service
            .revert_expired_pairings()
            .await
            .map(|_count| ())
    }

    async fn run_delivery_retries(&self) -> Result<(), AppError> {
        if self.app_state.is_shutting_down() {
            return Ok(());
        }
        self.app_state
            .channel_supervisor
            .retry_due_deliveries()
            .await
            .map(|_count| ())
    }

    async fn run_signal_timeouts(&self) -> Result<(), AppError> {
        if self.app_state.is_shutting_down() {
            return Ok(());
        }
        let expired = self
            .app_state
            .task_service
            .find_expired_signal_tasks()
            .await?;
        if expired.is_empty() {
            return Ok(());
        }
        let signal_service = self.app_state.signal_service();
        for task in expired {
            let is_continuous = matches!(
                task.kind,
                TaskKind::Signal {
                    mode: crate::agent::task::models::SignalMode::Continuous,
                    ..
                }
            );
            let outcome = if is_continuous {
                self.app_state
                    .task_service
                    .mark_completed(&task.id, Some("monitoring window ended".into()))
                    .await
                    .map(|_| ())
            } else {
                self.app_state
                    .task_service
                    .mark_failed(&task.id, "timed out: no matching signal".into())
                    .await
                    .map(|_| ())
            };
            if let Err(e) = outcome {
                tracing::warn!(
                    task_id = %task.id,
                    error = %e,
                    "Failed to transition expired signal task"
                );
                continue;
            }
            if let Some(ref svc) = signal_service {
                svc.unregister(&task.user_id, &task.id).await;
            }
        }
        Ok(())
    }

    async fn run_cron_tasks(&self) -> Result<(), AppError> {
        if self.app_state.is_shutting_down() {
            return Ok(());
        }
        let templates = self
            .app_state
            .task_service
            .find_due_cron_templates()
            .await?;
        if templates.is_empty() {
            return Ok(());
        }

        let server_tz = self.app_state.config.server.timezone.as_str();

        for template in templates {
            let (cron_expression, timezone) = match &template.kind {
                TaskKind::Cron {
                    cron_expression,
                    timezone,
                    ..
                } => (
                    cron_expression.clone(),
                    crate::auth::models::resolve_timezone(timezone.as_deref(), server_tz),
                ),
                _ => continue,
            };

            tracing::info!(
                task_id = %template.id,
                title = %template.title,
                timezone = %timezone,
                "Firing cron task"
            );

            let app_state = self.app_state.clone();
            let task_service = self.app_state.task_service.clone();
            let cron_expr = cron_expression.clone();
            let task_clone = template.clone();
            let tz_clone = timezone.clone();

            tokio::spawn(async move {
                if let Err(e) = execute_cron(&app_state, &task_clone).await {
                    tracing::error!(
                        error = %e,
                        task_id = %task_clone.id,
                        "Cron execution failed"
                    );
                }
                match next_cron_occurrence(&cron_expr, &tz_clone) {
                    Ok(next) => {
                        if let Err(e) = task_service
                            .advance_cron_template(&task_clone.id, next)
                            .await
                        {
                            tracing::warn!(error = %e, task_id = %task_clone.id, "Failed to advance cron template");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, task_id = %task_clone.id, "Failed to compute next cron occurrence");
                    }
                }
            });
        }

        Ok(())
    }

    async fn run_deferred_tasks(&self) -> Result<(), AppError> {
        if self.app_state.is_shutting_down() {
            return Ok(());
        }
        let tasks = self.app_state.task_service.find_deferred_due().await?;
        if tasks.is_empty() {
            return Ok(());
        }

        let executor = self.app_state.task_executor.clone();

        for task in tasks {
            tracing::info!(
                task_id = %task.id,
                title = %task.title,
                "Firing deferred task"
            );

            let exec = executor.clone();
            tokio::spawn(async move {
                if let Err(e) = exec.run_task(task).await {
                    tracing::warn!(error = %e, "Failed to run deferred task");
                }
            });
        }

        Ok(())
    }

    async fn run_heartbeats(&self) -> Result<(), AppError> {
        if self.app_state.is_shutting_down() {
            return Ok(());
        }
        let now = Utc::now();
        let agents = self
            .app_state
            .agent_service
            .find_due_heartbeats(now)
            .await?;
        if agents.is_empty() {
            return Ok(());
        }

        for agent in agents {
            let interval = match agent.heartbeat_interval {
                Some(mins) if mins > 0 => mins,
                _ => continue,
            };

            let user_id = agent.user_id.clone();
            let Some(user) = self.app_state.user_service.find_by_id(&user_id).await? else {
                tracing::warn!(agent_id = %agent.id, user_id = %user_id, "Heartbeat agent's user not found, skipping");
                continue;
            };
            let ws = self
                .app_state
                .storage_service
                .agent_workspace(&user.handle, &agent.handle);
            let heartbeat_content = match ws.read("HEARTBEAT.md") {
                Some(content) if !content.trim().is_empty() => content,
                _ => {
                    let next = now + chrono::Duration::minutes(interval as i64);
                    let _ = self
                        .app_state
                        .agent_service
                        .update_next_heartbeat(&agent.id, Some(next))
                        .await;
                    continue;
                }
            };

            tracing::info!(
                agent_id = %agent.id,
                "Firing heartbeat"
            );

            let app_state = self.app_state.clone();
            let agent_clone = agent.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    execute_heartbeat(&app_state, &agent_clone, &user_id, &heartbeat_content).await
                {
                    tracing::error!(
                        error = %e,
                        agent_id = %agent_clone.id,
                        "Heartbeat execution failed"
                    );
                }
                let next = Utc::now() + chrono::Duration::minutes(interval as i64);
                if let Err(e) = app_state
                    .agent_service
                    .update_next_heartbeat(&agent_clone.id, Some(next))
                    .await
                {
                    tracing::error!(
                        error = %e,
                        agent_id = %agent_clone.id,
                        "Failed to advance heartbeat"
                    );
                }
            });
        }

        Ok(())
    }

    async fn run_token_cleanup(&self) -> Result<(), AppError> {
        let deleted = self.app_state.token_service.cleanup_expired().await?;
        if deleted > 0 {
            tracing::info!(count = deleted, "Cleaned up expired tokens");
        }
        Ok(())
    }

    async fn run_share_cleanup(&self) -> Result<(), AppError> {
        let deleted = self.app_state.share_service.cleanup_expired().await?;
        if deleted > 0 {
            tracing::info!(count = deleted, "Cleaned up expired share links");
        }
        Ok(())
    }
}

pub async fn execute_cron(
    state: &AppState,
    template: &crate::agent::task::models::Task,
) -> Result<(), AppError> {
    use crate::agent::task::models::CronConcurrency;

    let concurrency = match &template.kind {
        TaskKind::Cron { concurrency, .. } => *concurrency,
        _ => return Ok(()),
    };

    let active = state
        .task_service
        .find_active_runs_by_cron(&template.id)
        .await
        .unwrap_or_default();

    if !active.is_empty() {
        match concurrency {
            CronConcurrency::Allow => {}
            CronConcurrency::Forbid => {
                tracing::info!(
                    template_id = %template.id,
                    active = active.len(),
                    "Cron concurrency=Forbid: skipping fire while previous still active"
                );
                return Ok(());
            }
            CronConcurrency::Replace => {
                for run in &active {
                    state.task_executor.cancel_task(&run.id).await;
                }
            }
        }
    }

    let next_sequence = state
        .task_service
        .find_runs_by_cron(&template.id)
        .await
        .map(|runs| runs.len() as u64 + 1)
        .unwrap_or(1);

    let run = state
        .task_service
        .spawn_cron_run(template, Utc::now(), next_sequence)
        .await?;

    let exec = state.task_executor.clone();
    let template_id = template.id.clone();
    tokio::spawn(async move {
        if let Err(e) = exec.run_task(run).await {
            tracing::warn!(error = %e, template_id = %template_id, "Failed to run CronRun execution");
        }
    });

    Ok(())
}

async fn execute_heartbeat(
    state: &AppState,
    agent: &Agent,
    user_id: &str,
    heartbeat_content: &str,
) -> Result<(), AppError> {
    let _execution = state.execution_registry.start(
        user_id,
        crate::core::execution::NewExecution {
            title: format!("{} heartbeat", agent.name),
            kind: crate::core::execution::ExecutionKind::Scheduled,
            action: Some("Running heartbeat".to_string()),
            source: Some(crate::core::execution::ExecutionSource {
                kind: crate::core::execution::ExecutionSourceKind::System,
                id: None,
            }),
            related_chat_ids: Vec::new(),
            can_cancel: false,
        },
    );
    let agent_id = &agent.id;

    let chat_id = if let Some(ref cid) = agent.heartbeat_chat_id {
        cid.clone()
    } else {
        let chat = state
            .chat_service
            .create_chat(
                user_id,
                CreateChatRequest {
                    space_id: None,
                    task_id: None,
                    agent_id: agent_id.clone(),
                    title: Some("Heartbeat".to_string()),
                    metadata: None,
                },
            )
            .await?;

        state
            .agent_service
            .update_heartbeat_chat(&agent.id, &chat.id)
            .await?;

        chat.id
    };

    let cancel_token = state.active_sessions.register(&chat_id).await;

    // No create_stream_user_message - heartbeats are transient. Persisting
    // shows them in the UI as if the user typed it, and they reinforce the
    // "respond conversationally" pattern in subsequent ticks.
    let agent_msg = state
        .chat_service
        .create_executing_agent_message(&chat_id, agent_id)
        .await?;
    let agent_msg_id = agent_msg.id.clone();

    let continuation_prompt = state.prompts.read("HEARTBEAT_TASK.md");

    let builder = Box::new(HeartbeatConversationBuilder {
        user_service: state.user_service.clone(),
        storage_service: state.storage_service.clone(),
        agent_service: state.agent_service.clone(),
        continuation_prompt,
        heartbeat_content: heartbeat_content.to_string(),
    });
    let result = state
        .harness
        .run_loop(
            user_id,
            &chat_id,
            &agent_msg_id,
            cancel_token,
            builder,
            &[],
            None,
        )
        .await;

    match result {
        Ok(crate::agent::harness::AgentLoopOutcome {
            inference,
            mut response,
        }) => match inference {
            InferenceResponse::Completed {
                text,
                attachments,
                reasoning,
                ..
            } => {
                response.content = text;
                response.attachments = attachments;
                response.reasoning = reasoning;
                let _ = state.chat_service.complete_agent_message(response).await;
            }
            InferenceResponse::Cancelled(text) => {
                response.content = text;
                let _ = state.chat_service.complete_agent_message(response).await;
            }
            InferenceResponse::ExternalToolPending { .. } => {
                tracing::warn!(chat_id = %chat_id, "Heartbeat agent hit external tool pending — not supported");
            }
            InferenceResponse::Handled => {
                // Command dispatch handled the response inline; nothing to do.
            }
        },
        Err(e) => {
            if let Ok(msg) = state.chat_service.get_message(user_id, &agent_msg_id).await {
                let _ = state
                    .chat_service
                    .fail_agent_message(msg, e.to_string())
                    .await;
            }
            tracing::error!(error = %e, chat_id = %chat_id, "Heartbeat agent tool loop failed");
        }
    }

    state.active_sessions.remove(&chat_id).await;
    Ok(())
}
