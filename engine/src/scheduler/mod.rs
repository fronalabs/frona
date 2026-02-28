use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::agent::execution::{self, AgentLoopOutcome};
use crate::agent::models::Agent;
use crate::agent::task::models::TaskKind;
use crate::api::repo::generic::SurrealRepo;
use crate::api::repo::insights::SurrealInsightRepo;
use crate::core::state::AppState;
use crate::chat::models::CreateChatRequest;
use crate::chat::repository::ChatRepository;
use crate::core::error::AppError;
use crate::inference::config::ModelGroup;
use crate::inference::InferenceResponse;
use crate::memory::insight::repository::InsightRepository;
use crate::memory::models::MemorySourceType;
use crate::space::repository::SpaceRepository;
use crate::tool::schedule::next_cron_occurrence;

pub struct Scheduler {
    app_state: AppState,
    compaction_model_group: ModelGroup,
}

macro_rules! spawn_periodic {
    ($scheduler:expr, $interval:expr, $name:expr, $method:ident) => {{
        let s = $scheduler.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep($interval).await;
                if let Err(e) = s.$method().await {
                    tracing::warn!(error = %e, task = $name, "Scheduled task failed");
                }
            }
        });
    }};
}

impl Scheduler {
    pub fn new(app_state: AppState, compaction_model_group: ModelGroup) -> Self {
        Self {
            app_state,
            compaction_model_group,
        }
    }

    pub fn start(self: Arc<Self>) {
        let cfg = &self.app_state.config;
        let space = Duration::from_secs(cfg.scheduler.space_compaction_secs);
        let insight = Duration::from_secs(cfg.scheduler.insight_compaction_secs);
        let poll = Duration::from_secs(cfg.scheduler.poll_secs);

        spawn_periodic!(self, space, "space_compaction", run_space_compaction);
        spawn_periodic!(self, insight, "insight_compaction", run_insight_compaction);
        spawn_periodic!(self, insight, "user_insight_compaction", run_user_insight_compaction);
        spawn_periodic!(self, poll, "poll_tasks", run_poll_tasks);
        spawn_periodic!(self, space, "token_cleanup", run_token_cleanup);
    }

    async fn run_poll_tasks(&self) -> Result<(), AppError> {
        self.run_cron_tasks().await?;
        self.run_deferred_tasks().await?;
        self.run_heartbeats().await
    }

    async fn run_cron_tasks(&self) -> Result<(), AppError> {
        let templates = self.app_state.task_service.find_due_cron_templates().await?;
        if templates.is_empty() {
            return Ok(());
        }

        for template in templates {
            let cron_expression = match &template.kind {
                TaskKind::Cron { cron_expression, .. } => cron_expression.clone(),
                _ => continue,
            };

            tracing::info!(
                task_id = %template.id,
                title = %template.title,
                "Firing cron task"
            );

            let app_state = self.app_state.clone();
            let task_service = self.app_state.task_service.clone();
            let cron_expr = cron_expression.clone();
            let task_clone = template.clone();

            tokio::spawn(async move {
                if let Err(e) = execute_cron(&app_state, &task_clone).await {
                    tracing::error!(
                        error = %e,
                        task_id = %task_clone.id,
                        "Cron execution failed"
                    );
                }
                match next_cron_occurrence(&cron_expr) {
                    Ok(next) => {
                        if let Err(e) = task_service
                            .advance_cron_template(&task_clone.id, next, task_clone.chat_id.as_deref())
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
        let tasks = self.app_state.task_service.find_deferred_due().await?;
        if tasks.is_empty() {
            return Ok(());
        }

        let executor = match self.app_state.task_executor() {
            Some(e) => e,
            None => {
                tracing::warn!("Task executor not available, skipping deferred tasks");
                return Ok(());
            }
        };

        for task in tasks {
            tracing::info!(
                task_id = %task.id,
                title = %task.title,
                "Firing deferred task"
            );

            if let Err(e) = executor.spawn_execution(task).await {
                tracing::warn!(error = %e, "Failed to spawn deferred task");
            }
        }

        Ok(())
    }

    async fn run_heartbeats(&self) -> Result<(), AppError> {
        let now = Utc::now();
        let agents = self.app_state.agent_service.find_due_heartbeats(now).await?;
        if agents.is_empty() {
            return Ok(());
        }

        for agent in agents {
            let interval = match agent.heartbeat_interval {
                Some(mins) if mins > 0 => mins,
                _ => continue,
            };

            let user_id = match &agent.user_id {
                Some(uid) => uid.clone(),
                None => continue,
            };

            let ws = self.app_state.agent_workspaces.get(&agent.id);
            let heartbeat_content = match ws.read("HEARTBEAT.md") {
                Some(content) if !content.trim().is_empty() => content,
                _ => {
                    let next = now + chrono::Duration::minutes(interval as i64);
                    let _ = self.app_state.agent_service.update_next_heartbeat(&agent.id, Some(next)).await;
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
                if let Err(e) = execute_heartbeat(&app_state, &agent_clone, &user_id, &heartbeat_content).await {
                    tracing::error!(
                        error = %e,
                        agent_id = %agent_clone.id,
                        "Heartbeat execution failed"
                    );
                }
                let next = Utc::now() + chrono::Duration::minutes(interval as i64);
                if let Err(e) = app_state.agent_service.update_next_heartbeat(&agent_clone.id, Some(next)).await {
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

    async fn run_space_compaction(&self) -> Result<(), AppError> {
        let space_repo: SurrealRepo<crate::space::models::Space> =
            SurrealRepo::new(self.app_state.db.clone());
        let chat_repo: SurrealRepo<crate::chat::models::Chat> =
            SurrealRepo::new(self.app_state.db.clone());

        let spaces = space_repo.find_all().await?;

        for space in spaces {
            let chats = chat_repo.find_by_space_id(&space.id).await?;
            if chats.is_empty() {
                continue;
            }

            let mut summaries = Vec::new();
            for chat in &chats {
                let title = chat.title.clone().unwrap_or_else(|| "Untitled".to_string());

                let memory = self
                    .app_state
                    .memory_service
                    .get_memory(MemorySourceType::Chat, &chat.id)
                    .await?;

                let summary = if let Some(mem) = memory {
                    mem.content
                } else {
                    format!("(No summary available for chat: {title})")
                };

                summaries.push((title, summary));
            }

            if let Err(e) = self
                .app_state
                .memory_service
                .compact_space(&space.id, summaries, &self.compaction_model_group)
                .await
            {
                tracing::warn!(
                    space_id = %space.id,
                    error = %e,
                    "Failed to compact space"
                );
            }
        }

        Ok(())
    }

    async fn run_insight_compaction(&self) -> Result<(), AppError> {
        let repo: SurrealInsightRepo = SurrealRepo::new(self.app_state.db.clone());
        let ids = repo.find_distinct_agent_ids().await?;
        self.run_insight_compaction_for("agent", ids).await
    }

    async fn run_user_insight_compaction(&self) -> Result<(), AppError> {
        let repo: SurrealInsightRepo = SurrealRepo::new(self.app_state.db.clone());
        let ids = repo.find_distinct_user_ids().await?;
        self.run_insight_compaction_for("user", ids).await
    }

    async fn run_insight_compaction_for(
        &self,
        kind: &str,
        ids: Vec<String>,
    ) -> Result<(), AppError> {
        tracing::info!(count = ids.len(), kind = kind, "Starting scheduled insight compaction");
        for id in &ids {
            tracing::info!(%id, kind = kind, "Running scheduled insight compaction");
            let result = match kind {
                "agent" => {
                    self.app_state
                        .memory_service
                        .compact_insights_if_needed(id, &self.compaction_model_group)
                        .await
                }
                "user" => {
                    self.app_state
                        .memory_service
                        .compact_user_insights_if_needed(id, &self.compaction_model_group)
                        .await
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                tracing::warn!(%id, kind = kind, error = %e, "Failed to compact insights");
            }
        }
        Ok(())
    }
}

async fn execute_cron(
    state: &AppState,
    task: &crate::agent::task::models::Task,
) -> Result<(), AppError> {
    let user_id = &task.user_id;
    let agent_id = &task.agent_id;

    let chat_id = if let Some(ref cid) = task.chat_id {
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
                    title: Some(format!("Cron: {}", task.title)),
                },
            )
            .await?;

        let _ = state
            .task_service
            .advance_cron_template(&task.id, Utc::now(), Some(&chat.id))
            .await;

        chat.id
    };

    execute_background_agent(state, user_id, &chat_id, &task.description).await
}

async fn execute_heartbeat(
    state: &AppState,
    agent: &Agent,
    user_id: &str,
    heartbeat_content: &str,
) -> Result<(), AppError> {
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
                },
            )
            .await?;

        state
            .agent_service
            .update_heartbeat_chat(&agent.id, &chat.id)
            .await?;

        chat.id
    };

    let message = format!(
        "Heartbeat: review and act on your checklist.\n\n{}",
        heartbeat_content
    );
    execute_background_agent(state, user_id, &chat_id, &message).await
}

async fn execute_background_agent(
    state: &AppState,
    user_id: &str,
    chat_id: &str,
    message_content: &str,
) -> Result<(), AppError> {
    state
        .chat_service
        .create_stream_user_message(user_id, chat_id, message_content, vec![])
        .await?;

    let cancel_token = state.active_sessions.register(chat_id).await;

    let result = execution::run_agent_loop(
        state, user_id, chat_id, cancel_token,
    )
    .await;

    match result {
        Ok(AgentLoopOutcome { response, accumulated_text, .. }) => {
            if let InferenceResponse::Completed { attachments, .. } = response
                && !accumulated_text.is_empty()
            {
                let _ = state
                    .chat_service
                    .save_assistant_message_with_tool_calls(
                        chat_id, accumulated_text, None, attachments,
                    )
                    .await;
            }
        }
        Err(e) => {
            tracing::error!(error = %e, chat_id = %chat_id, "Background agent tool loop failed");
        }
    }

    state.active_sessions.remove(chat_id).await;
    Ok(())
}
