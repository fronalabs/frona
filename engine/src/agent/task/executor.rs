use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::execution;
use crate::agent::task::models::{Task, TaskKind, TaskStatus};
use crate::chat::dto::CreateChatRequest;
use crate::chat::message::models::MessageTool;
use crate::error::AppError;
use crate::llm::tool_loop::ToolLoopOutcome;

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
    app_state: crate::api::state::AppState,
}

impl TaskExecutor {
    pub fn new(
        task_service: TaskService,
        chat_service: crate::chat::service::ChatService,
        broadcast_service: crate::chat::broadcast::BroadcastService,
        app_state: crate::api::state::AppState,
        max_global_concurrent: usize,
    ) -> Self {
        Self {
            task_service,
            state: Arc::new(ExecutorState {
                chat_service,
                broadcast_service,
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

        let result = execution::run_agent_loop(
            &self.state.app_state,
            &agent_id,
            &user_id,
            &chat_id,
            task.space_id.as_deref(),
            cancel_token,
        )
        .await;

        match result {
            Ok(execution::AgentLoopOutcome {
                tool_loop_outcome,
                accumulated_text,
                last_segment,
            }) => match tool_loop_outcome {
                ToolLoopOutcome::Completed { attachments, .. } => {
                    if !accumulated_text.is_empty() {
                        let _ = self
                            .state
                            .chat_service
                            .save_assistant_message_with_tool_calls(
                                &chat_id, accumulated_text.clone(), None, attachments,
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
                        let result_text = if last_segment.is_empty() { &accumulated_text } else { &last_segment };
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
                    if !accumulated_text.is_empty() {
                        let _ = self
                            .state
                            .chat_service
                            .save_assistant_message(&chat_id, accumulated_text)
                            .await;
                    }

                    self.task_service.mark_cancelled(&task_id).await?;
                    self.broadcast_task_status(&task, "cancelled");
                }
                ToolLoopOutcome::ExternalToolPending {
                    accumulated_text: ext_text,
                    tool_calls_json,
                    tool_results,
                    external_tool,
                } => {
                    let _ = self
                        .state
                        .chat_service
                        .save_assistant_message_with_tool_calls(
                            &chat_id,
                            ext_text,
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
            Err(e) => {
                let error_msg = format!("Task execution error: {}", e);
                tracing::error!(error = %e, task_id = %task_id, "Task execution failed");
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
