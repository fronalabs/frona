use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::agent::models::Agent;
use crate::agent::task::models::TaskKind;
use crate::agent::task::service::TaskService;
use crate::api::repo::chats::SurrealChatRepo;
use crate::api::repo::insights::SurrealInsightRepo;
use crate::api::repo::spaces::SurrealSpaceRepo;
use crate::api::state::AppState;
use crate::chat::dto::CreateChatRequest;
use crate::chat::repository::ChatRepository;
use crate::error::AppError;
use crate::llm::config::ModelGroup;
use crate::llm::convert::to_rig_messages;
use crate::llm::tool_loop::{self, ToolLoopEvent, ToolLoopEventKind, ToolLoopOutcome};
use crate::repository::Repository;
use crate::memory::insight::repository::InsightRepository;
use crate::memory::models::MemorySourceType;
use crate::memory::service::MemoryService;
use crate::space::repository::SpaceRepository;
use crate::tool::schedule::next_cron_occurrence;

pub struct Scheduler {
    memory_service: MemoryService,
    space_repo: SurrealSpaceRepo,
    chat_repo: SurrealChatRepo,
    insight_repo: SurrealInsightRepo,
    compaction_model_group: ModelGroup,
    interval: Duration,
    task_service: TaskService,
    app_state: AppState,
}

impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory_service: MemoryService,
        space_repo: SurrealSpaceRepo,
        chat_repo: SurrealChatRepo,
        insight_repo: SurrealInsightRepo,
        compaction_model_group: ModelGroup,
        interval: Duration,
        task_service: TaskService,
        app_state: AppState,
    ) -> Self {
        Self {
            memory_service,
            space_repo,
            chat_repo,
            insight_repo,
            compaction_model_group,
            interval,
            task_service,
            app_state,
        }
    }

    pub fn start(self: Arc<Self>) {
        let scheduler = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(scheduler.interval).await;
                if let Err(e) = scheduler.run_space_compaction().await {
                    tracing::warn!(error = %e, "Scheduled space compaction failed");
                }
            }
        });

        let scheduler = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(7200)).await;
                if let Err(e) = scheduler.run_insight_compaction().await {
                    tracing::warn!(error = %e, "Scheduled insight compaction failed");
                }
            }
        });

        let scheduler = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(7200)).await;
                if let Err(e) = scheduler.run_user_insight_compaction().await {
                    tracing::warn!(error = %e, "Scheduled user insight compaction failed");
                }
            }
        });

        let scheduler = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if let Err(e) = scheduler.run_cron_tasks().await {
                    tracing::warn!(error = %e, "Cron task check failed");
                }
                if let Err(e) = scheduler.run_deferred_tasks().await {
                    tracing::warn!(error = %e, "Deferred task check failed");
                }
                if let Err(e) = scheduler.run_heartbeats().await {
                    tracing::warn!(error = %e, "Heartbeat check failed");
                }
            }
        });
    }

    async fn run_cron_tasks(&self) -> Result<(), AppError> {
        let templates = self.task_service.find_due_cron_templates().await?;
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
            let task_service = self.task_service.clone();
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
        let tasks = self.task_service.find_deferred_due().await?;
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

    async fn run_space_compaction(&self) -> Result<(), AppError> {
        let spaces = self.space_repo.find_all().await?;

        for space in spaces {
            let chats = self.chat_repo.find_by_space_id(&space.id).await?;
            if chats.is_empty() {
                continue;
            }

            let mut summaries = Vec::new();
            for chat in &chats {
                let title = chat.title.clone().unwrap_or_else(|| "Untitled".to_string());

                let memory = self
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
        let agent_ids = self.insight_repo.find_distinct_agent_ids().await?;

        tracing::info!(
            agent_count = agent_ids.len(),
            "Starting scheduled insight compaction"
        );

        for agent_id in &agent_ids {
            tracing::info!(agent_id = %agent_id, "Running scheduled insight compaction for agent");
            if let Err(e) = self
                .memory_service
                .compact_insights_if_needed(agent_id, &self.compaction_model_group)
                .await
            {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "Failed to compact insights for agent"
                );
            }
        }

        Ok(())
    }

    async fn run_user_insight_compaction(&self) -> Result<(), AppError> {
        let user_ids = self.insight_repo.find_distinct_user_ids().await?;

        tracing::info!(
            user_count = user_ids.len(),
            "Starting scheduled user insight compaction"
        );

        for user_id in &user_ids {
            tracing::info!(user_id = %user_id, "Running scheduled user insight compaction");
            if let Err(e) = self
                .memory_service
                .compact_user_insights_if_needed(user_id, &self.compaction_model_group)
                .await
            {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Failed to compact insights for user"
                );
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

    execute_background_agent(state, agent_id, user_id, &chat_id, &task.description).await
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
    execute_background_agent(state, agent_id, user_id, &chat_id, &message).await
}

async fn execute_background_agent(
    state: &AppState,
    agent_id: &str,
    user_id: &str,
    chat_id: &str,
    message_content: &str,
) -> Result<(), AppError> {
    state
        .chat_service
        .create_stream_user_message(user_id, chat_id, message_content, vec![])
        .await?;

    let agent_config = state
        .chat_service
        .resolve_agent_config(agent_id)
        .await?;

    let skill_summaries: Vec<(String, String)> = state
        .skill_resolver
        .list(agent_id)
        .await
        .into_iter()
        .map(|s| (s.name, s.description))
        .collect();

    let agent_summaries = crate::api::routes::messages::build_agent_summaries_from_state(
        state, user_id, agent_id, &agent_config.tools,
    )
    .await;

    let system_prompt = state
        .memory_service
        .build_augmented_system_prompt(
            &agent_config.system_prompt,
            agent_id,
            user_id,
            None,
            &skill_summaries,
            &agent_summaries,
            &agent_config.identity,
        )
        .await
        .unwrap_or(agent_config.system_prompt.clone());

    let model_group = state
        .chat_service
        .provider_registry()
        .resolve_model_group(&agent_config.model_group)
        .map_err(|e| AppError::Llm(e.to_string()))?;

    let registry = state.chat_service.provider_registry().clone();

    let stored_messages = state.chat_service.get_stored_messages(chat_id).await;
    let rig_history = to_rig_messages(&stored_messages, agent_id);

    let (tool_event_tx, mut tool_event_rx) = tokio::sync::mpsc::channel::<ToolLoopEvent>(32);

    let tool_registry = crate::api::routes::messages::build_tool_registry(
        state,
        agent_id,
        user_id,
        chat_id,
        &agent_config.tools,
        agent_config.sandbox_config.as_ref(),
    )
    .await;

    let user = state.user_repo.find_by_id(user_id).await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    let agent = state.agent_service.find_by_id(agent_id).await?
        .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;
    let chat = state.chat_service.find_chat(chat_id).await?
        .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;
    let tool_ctx = crate::tool::ToolContext {
        user,
        agent,
        chat,
        event_tx: tool_event_tx.clone(),
    };

    let cancel_token = state.active_sessions.register(chat_id).await;

    let tool_handle = {
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            tool_loop::run_tool_loop(
                &registry,
                &model_group,
                &system_prompt,
                rig_history,
                &tool_registry,
                tool_event_tx,
                cancel_token,
                &tool_ctx,
            )
            .await
        })
    };

    let mut accumulated = String::new();
    while let Some(event) = tool_event_rx.recv().await {
        if let ToolLoopEventKind::Text(text) = event.kind {
            accumulated.push_str(&text);
        }
    }

    match tool_handle.await {
        Ok(Ok(outcome)) => {
            if let ToolLoopOutcome::Completed { text: _, attachments } = outcome
                && !accumulated.is_empty()
            {
                let _ = state
                    .chat_service
                    .save_assistant_message_with_tool_calls(
                        chat_id, accumulated, None, attachments,
                    )
                    .await;
            }
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, chat_id = %chat_id, "Background agent tool loop failed");
        }
        Err(e) => {
            tracing::error!(error = %e, chat_id = %chat_id, "Background agent tool loop panicked");
        }
    }

    state.active_sessions.remove(chat_id).await;
    Ok(())
}
