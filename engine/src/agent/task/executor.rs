use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::task::models::{Task, TaskKind, TaskStatus};
use crate::chat::dto::CreateChatRequest;
use crate::chat::message::models::MessageTool;
use crate::error::AppError;
use crate::repository::Repository;
use crate::llm::convert::to_rig_messages;
use crate::llm::tool_loop::{self, ToolLoopEvent, ToolLoopEventKind, ToolLoopOutcome};

use super::service::TaskService;

pub struct TaskExecutor {
    task_service: TaskService,
    state: Arc<ExecutorState>,
    active_tasks: Mutex<HashMap<String, CancellationToken>>,
    max_global_concurrent: usize,
}

struct ExecutorState {
    chat_service: crate::chat::service::ChatService,
    broadcast_service: crate::chat::broadcast::BroadcastService,
    memory_service: crate::memory::service::MemoryService,
    skill_resolver: crate::agent::skill::resolver::SkillResolver,
    app_state: crate::api::state::AppState,
}

impl TaskExecutor {
    pub fn new(
        task_service: TaskService,
        chat_service: crate::chat::service::ChatService,
        broadcast_service: crate::chat::broadcast::BroadcastService,
        memory_service: crate::memory::service::MemoryService,
        skill_resolver: crate::agent::skill::resolver::SkillResolver,
        app_state: crate::api::state::AppState,
        max_global_concurrent: usize,
    ) -> Self {
        Self {
            task_service,
            state: Arc::new(ExecutorState {
                chat_service,
                broadcast_service,
                memory_service,
                skill_resolver,
                app_state,
            }),
            active_tasks: Mutex::new(HashMap::new()),
            max_global_concurrent,
        }
    }

    pub async fn resume_all(self: &Arc<Self>) {
        let tasks = match self.task_service.find_resumable().await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::error!(error = %e, "Failed to query resumable tasks");
                return;
            }
        };

        if tasks.is_empty() {
            return;
        }

        tracing::info!(count = tasks.len(), "Resuming tasks from previous run");

        for task in tasks {
            if task.status == TaskStatus::Cancelled {
                continue;
            }

            if let Err(e) = self.spawn_execution(task).await {
                tracing::warn!(error = %e, "Failed to spawn task during resume");
            }
        }
    }

    pub async fn spawn_execution(self: &Arc<Self>, task: Task) -> Result<(), AppError> {
        let active = self.active_tasks.lock().await;
        if active.len() >= self.max_global_concurrent {
            tracing::info!(
                task_id = %task.id,
                active = active.len(),
                limit = self.max_global_concurrent,
                "Global concurrency limit reached, task stays Pending"
            );
            return Ok(());
        }

        let agent_max = self.get_agent_concurrent_limit(&task.agent_id).await;
        let agent_active_count = active
            .keys()
            .filter(|k| k.starts_with(&format!("{}:", task.agent_id)))
            .count();

        if agent_active_count >= agent_max {
            tracing::info!(
                task_id = %task.id,
                agent_id = %task.agent_id,
                active = agent_active_count,
                limit = agent_max,
                "Per-agent concurrency limit reached, task stays Pending"
            );
            return Ok(());
        }
        drop(active);

        let cancel_token = CancellationToken::new();
        let key = format!("{}:{}", task.agent_id, task.id);
        self.active_tasks
            .lock()
            .await
            .insert(key.clone(), cancel_token.clone());

        let executor = Arc::clone(self);
        let task_id = task.id.clone();

        tokio::spawn(async move {
            let result = executor.execute_task(task, cancel_token).await;
            executor.active_tasks.lock().await.remove(&key);

            if let Err(e) = result {
                tracing::error!(error = %e, task_id = %task_id, "Task execution failed");
            }
        });

        Ok(())
    }

    pub async fn cancel_task(&self, task_id: &str) -> bool {
        let active = self.active_tasks.lock().await;
        for (key, token) in active.iter() {
            if key.ends_with(&format!(":{}", task_id)) {
                token.cancel();
                return true;
            }
        }
        false
    }

    async fn get_agent_concurrent_limit(&self, agent_id: &str) -> usize {
        use crate::api::repo::generic::SurrealRepo;
        use crate::repository::Repository;

        let repo: SurrealRepo<crate::agent::models::Agent> = SurrealRepo::new(
            self.state.app_state.db.clone(),
        );

        if let Ok(Some(agent)) = repo.find_by_id(agent_id).await {
            return agent.max_concurrent_tasks.unwrap_or(3) as usize;
        }

        3
    }

    async fn execute_task(
        self: &Arc<Self>,
        mut task: Task,
        cancel_token: CancellationToken,
    ) -> Result<(), AppError> {
        let task_id = task.id.clone();
        let user_id = task.user_id.clone();
        let agent_id = task.agent_id.clone();

        let current_status = self
            .task_service
            .find_by_id(&task_id)
            .await?
            .map(|t| t.status);

        if matches!(current_status, Some(TaskStatus::Cancelled)) {
            tracing::info!(task_id = %task_id, "Task already cancelled, skipping");
            return Ok(());
        }

        let chat_id = if let Some(ref cid) = task.chat_id {
            cid.clone()
        } else {
            let chat = self
                .state
                .chat_service
                .create_chat(
                    &user_id,
                    CreateChatRequest {
                        space_id: task.space_id.clone(),
                        task_id: Some(task_id.clone()),
                        agent_id: agent_id.clone(),
                        title: Some(format!("Task: {}", task.title)),
                    },
                )
                .await?;
            task.chat_id = Some(chat.id.clone());
            chat.id
        };

        self.task_service
            .mark_in_progress(&task_id, Some(&chat_id))
            .await?;

        self.broadcast_task_status(&task, "inprogress");

        let stored_messages = self.state.chat_service.get_stored_messages(&chat_id).await;
        let is_first_run = stored_messages.is_empty();

        if is_first_run {
            let source_agent_id = match &task.kind {
                TaskKind::Delegation { source_agent_id, .. } => source_agent_id.as_str(),
                _ => &task.agent_id,
            };
            self.state
                .chat_service
                .save_agent_message(&chat_id, source_agent_id, task.description.clone())
                .await?;
        }

        let agent_config = self
            .state
            .chat_service
            .resolve_agent_config(&agent_id)
            .await?;

        let skill_summaries: Vec<(String, String)> = self
            .state
            .skill_resolver
            .list(&agent_id)
            .await
            .into_iter()
            .map(|s| (s.name, s.description))
            .collect();

        let agent_summaries = crate::api::routes::messages::build_agent_summaries_from_state(
            &self.state.app_state,
            &user_id,
            &agent_id,
            &agent_config.tools,
        )
        .await;

        let system_prompt = self
            .state
            .memory_service
            .build_augmented_system_prompt(
                &agent_config.system_prompt,
                &agent_id,
                &user_id,
                task.space_id.as_deref(),
                &skill_summaries,
                &agent_summaries,
                &agent_config.identity,
            )
            .await
            .unwrap_or(agent_config.system_prompt.clone());

        let model_group = self
            .state
            .chat_service
            .provider_registry()
            .resolve_model_group(&agent_config.model_group)
            .map_err(|e| AppError::Llm(e.to_string()))?;

        let registry = self.state.chat_service.provider_registry().clone();

        let stored_messages = self.state.chat_service.get_stored_messages(&chat_id).await;
        let rig_history = to_rig_messages(&stored_messages, &agent_id);

        let (tool_event_tx, mut tool_event_rx) = tokio::sync::mpsc::channel::<ToolLoopEvent>(32);

        let tool_registry = crate::api::routes::messages::build_tool_registry(
            &self.state.app_state,
            &agent_id,
            &user_id,
            &chat_id,
            &agent_config.tools,
            agent_config.sandbox_config.as_ref(),
        )
        .await;

        let chat_obj = self.state.chat_service.find_chat(&chat_id).await?
            .ok_or_else(|| AppError::NotFound("Chat not found".into()))?;
        let user = self.state.app_state.user_repo.find_by_id(&user_id).await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;
        let agent_obj = self.state.app_state.agent_service.find_by_id(&agent_id).await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;
        let tool_ctx = crate::tool::ToolContext {
            user,
            agent: agent_obj,
            chat: chat_obj,
            event_tx: tool_event_tx.clone(),
        };

        let tool_handle = {
            let registry = registry.clone();
            let model_group = model_group.clone();
            let system_prompt = system_prompt.clone();
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
        let mut last_segment = String::new();
        while let Some(event) = tool_event_rx.recv().await {
            match event.kind {
                ToolLoopEventKind::Text(text) => {
                    accumulated.push_str(&text);
                    last_segment.push_str(&text);
                }
                ToolLoopEventKind::ToolCall { .. } | ToolLoopEventKind::ToolResult { .. } => {
                    last_segment.clear();
                }
                _ => {}
            }
        }

        match tool_handle.await {
            Ok(Ok(outcome)) => match outcome {
                ToolLoopOutcome::Completed { text: _, attachments } => {
                    if !accumulated.is_empty() {
                        let _ = self
                            .state
                            .chat_service
                            .save_assistant_message_with_tool_calls(
                                &chat_id, accumulated.clone(), None, attachments,
                            )
                            .await;
                    }

                    let children = self
                        .task_service
                        .find_by_source_chat_id(&chat_id)
                        .await
                        .unwrap_or_default();

                    let has_incomplete_children = children
                        .iter()
                        .any(|c| matches!(c.status, TaskStatus::Pending | TaskStatus::InProgress));

                    if has_incomplete_children {
                        tracing::info!(
                            task_id = %task_id,
                            incomplete_children = children.iter().filter(|c| matches!(c.status, TaskStatus::Pending | TaskStatus::InProgress)).count(),
                            "Task has incomplete children, staying InProgress"
                        );
                    } else {
                        let result_text = if last_segment.is_empty() { &accumulated } else { &last_segment };
                        let summary = if result_text.is_empty() {
                            None
                        } else {
                            Some(result_text.to_string())
                        };

                        self.task_service
                            .mark_completed(&task_id, summary.clone())
                            .await?;

                        self.deliver_result_to_source(&task, summary.as_deref())
                            .await;

                        self.broadcast_task_status_with_summary(
                            &task,
                            "completed",
                            summary.as_deref(),
                        );
                    }
                }
                ToolLoopOutcome::Cancelled(_) => {
                    if !accumulated.is_empty() {
                        let _ = self
                            .state
                            .chat_service
                            .save_assistant_message(&chat_id, accumulated)
                            .await;
                    }

                    self.task_service.mark_cancelled(&task_id).await?;
                    self.broadcast_task_status(&task, "cancelled");
                }
                ToolLoopOutcome::ExternalToolPending {
                    accumulated_text,
                    tool_calls_json,
                    tool_results,
                    external_tool,
                } => {
                    let _ = self
                        .state
                        .chat_service
                        .save_assistant_message_with_tool_calls(
                            &chat_id,
                            accumulated_text,
                            Some(tool_calls_json),
                            vec![],
                        )
                        .await;

                    for tr in &tool_results {
                        if tr.tool_data.is_some() {
                            let _ = self
                                .state
                                .chat_service
                                .save_tool_result_message_with_tool(
                                    &chat_id,
                                    &tr.tool_call_id,
                                    tr.result.clone(),
                                    tr.tool_data.clone(),
                                )
                                .await;
                        } else {
                            let _ = self
                                .state
                                .chat_service
                                .save_tool_result_message(
                                    &chat_id,
                                    &tr.tool_call_id,
                                    tr.result.clone(),
                                )
                                .await;
                        }
                    }

                    let _ = self
                        .state
                        .chat_service
                        .save_tool_result_message_with_tool(
                            &chat_id,
                            &external_tool.tool_call_id,
                            external_tool.result,
                            external_tool.tool_data,
                        )
                        .await;
                }
            },
            Ok(Err(e)) => {
                let error_msg = format!("Task execution error: {}", e);
                tracing::error!(error = %e, task_id = %task_id, "Task execution failed");
                self.task_service
                    .mark_failed(&task_id, error_msg)
                    .await?;
                self.deliver_error_to_source(&task, &e.to_string()).await;
                self.broadcast_task_status(&task, "failed");
            }
            Err(e) => {
                let error_msg = format!("Task panicked: {}", e);
                tracing::error!(error = %e, task_id = %task_id, "Task execution panicked");
                self.task_service
                    .mark_failed(&task_id, error_msg)
                    .await?;
                self.deliver_error_to_source(&task, &e.to_string()).await;
                self.broadcast_task_status(&task, "failed");
            }
        }

        Ok(())
    }

    async fn deliver_result_to_source(&self, task: &Task, summary: Option<&str>) {
        let TaskKind::Delegation {
            ref source_chat_id,
            deliver_directly,
            ..
        } = task.kind
        else {
            return;
        };

        let message_text = summary.unwrap_or_default().to_string();

        let attachments = if let Some(ref chat_id) = task.chat_id {
            self.state
                .chat_service
                .find_attachments_by_chat_id(chat_id)
                .await
                .unwrap_or_default()
        } else {
            vec![]
        };

        match self
            .state
            .chat_service
            .save_task_completion_message(
                source_chat_id,
                &task.agent_id,
                message_text,
                MessageTool::TaskCompletion {
                    task_id: task.id.clone(),
                    chat_id: task.chat_id.clone(),
                    status: TaskStatus::Completed,
                },
                attachments,
            )
            .await
        {
            Ok(msg) => {
                self.state.broadcast_service.broadcast_chat_message(
                    &task.user_id,
                    source_chat_id,
                    msg,
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, task_id = %task.id, "Failed to save task result to source chat");
            }
        }

        if !deliver_directly {
            self.check_and_resume_parent(source_chat_id, &task.user_id)
                .await;
        }
    }

    async fn deliver_error_to_source(&self, task: &Task, error: &str) {
        let TaskKind::Delegation {
            ref source_chat_id,
            deliver_directly,
            ..
        } = task.kind
        else {
            return;
        };

        let message_text = error.to_string();

        match self
            .state
            .chat_service
            .save_task_completion_message(
                source_chat_id,
                &task.agent_id,
                message_text,
                MessageTool::TaskCompletion {
                    task_id: task.id.clone(),
                    chat_id: task.chat_id.clone(),
                    status: TaskStatus::Failed,
                },
                vec![],
            )
            .await
        {
            Ok(msg) => {
                self.state.broadcast_service.broadcast_chat_message(
                    &task.user_id,
                    source_chat_id,
                    msg,
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, task_id = %task.id, "Failed to save task error to source chat");
            }
        }

        if !deliver_directly {
            self.check_and_resume_parent(source_chat_id, &task.user_id)
                .await;
        }
    }

    async fn check_and_resume_parent(&self, source_chat_id: &str, user_id: &str) {
        let siblings = match self
            .task_service
            .find_by_source_chat_id(source_chat_id)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to query sibling tasks");
                return;
            }
        };

        let all_done = siblings.iter().all(|t| {
            matches!(
                t.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        });

        if !all_done {
            tracing::debug!(
                source_chat_id = %source_chat_id,
                remaining = siblings.iter().filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::InProgress)).count(),
                "Not all sibling tasks done yet"
            );
            return;
        }

        tracing::info!(
            source_chat_id = %source_chat_id,
            "All child tasks complete, resuming parent tool loop"
        );

        let state = self.state.app_state.clone();
        let user_id = user_id.to_string();
        let chat_id = source_chat_id.to_string();
        tokio::spawn(async move {
            if let Err(e) =
                crate::api::routes::messages::resume_tool_loop_background(&state, &user_id, &chat_id)
                    .await
            {
                tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume parent tool loop");
            }
        });
    }

    fn broadcast_task_status(&self, task: &Task, status: &str) {
        let source_chat_id = match &task.kind {
            TaskKind::Delegation {
                source_chat_id, ..
            } => Some(source_chat_id.as_str()),
            _ => None,
        };

        self.state.broadcast_service.broadcast_task_update(
            &task.user_id,
            &task.id,
            status,
            &task.title,
            task.chat_id.as_deref(),
            source_chat_id,
            None,
        );
    }

    fn broadcast_task_status_with_summary(
        &self,
        task: &Task,
        status: &str,
        summary: Option<&str>,
    ) {
        let source_chat_id = match &task.kind {
            TaskKind::Delegation {
                source_chat_id, ..
            } => Some(source_chat_id.as_str()),
            _ => None,
        };

        self.state.broadcast_service.broadcast_task_update(
            &task.user_id,
            &task.id,
            status,
            &task.title,
            task.chat_id.as_deref(),
            source_chat_id,
            summary,
        );
    }
}
