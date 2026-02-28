use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::execution;
use crate::agent::task::models::{Task, TaskKind, TaskStatus};
use crate::api::files::Attachment;
use crate::core::state::AppState;
use crate::chat::models::CreateChatRequest;
use crate::chat::message::models::MessageTool;
use crate::core::error::AppError;
use crate::inference::InferenceResponse;

pub struct TaskExecutor {
    app_state: AppState,
    active_tasks: Mutex<HashMap<String, CancellationToken>>,
}

impl TaskExecutor {
    pub fn new(app_state: AppState) -> Self {
        Self {
            app_state,
            active_tasks: Mutex::new(HashMap::new()),
        }
    }

    pub async fn resume_all(self: &Arc<Self>) {
        let tasks = match self.app_state.task_service.find_resumable().await {
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
        if active.len() >= self.app_state.max_concurrent_tasks {
            tracing::info!(
                task_id = %task.id,
                active = active.len(),
                limit = self.app_state.max_concurrent_tasks,
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
        use crate::core::repository::Repository;

        let repo: SurrealRepo<crate::agent::models::Agent> = SurrealRepo::new(
            self.app_state.db.clone(),
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

        let current_status = self
            .app_state
            .task_service
            .find_by_id(&task_id)
            .await?
            .map(|t| t.status);

        if matches!(current_status, Some(TaskStatus::Cancelled)) {
            tracing::info!(task_id = %task_id, "Task already cancelled, skipping");
            return Ok(());
        }

        let chat_id = self.ensure_task_chat(&mut task).await?;

        self.app_state
            .task_service
            .mark_in_progress(&task_id, Some(&chat_id))
            .await?;

        self.broadcast_task_status(&task, "inprogress", None);
        self.save_initial_message_if_needed(&task, &chat_id).await?;

        let result = execution::run_agent_loop(
            &self.app_state,
            &task.user_id,
            &chat_id,
            cancel_token,
        )
        .await;

        match result {
            Ok(execution::AgentLoopOutcome {
                response,
                accumulated_text,
                last_segment,
            }) => {
                match response {
                    InferenceResponse::Completed { attachments, .. } => {
                        self.handle_completed(&task, &chat_id, accumulated_text, last_segment, attachments)
                            .await?;
                    }
                    InferenceResponse::Cancelled(_) => {
                        self.handle_cancelled(&task, &chat_id, accumulated_text).await?;
                    }
                    InferenceResponse::ExternalToolPending {
                        accumulated_text: ext_text,
                        tool_calls_json,
                        tool_results,
                        external_tool,
                        system_prompt: _,
                    } => {
                        let _ = self
                            .app_state
                            .chat_service
                            .save_external_tool_pending(
                                &chat_id, ext_text, tool_calls_json, &tool_results, external_tool,
                            )
                            .await;
                    }
                }
            }
            Err(e) => {
                self.handle_error(&task, &e).await?;
            }
        }

        Ok(())
    }

    pub async fn ensure_task_chat(&self, task: &mut Task) -> Result<String, AppError> {
        if let Some(ref cid) = task.chat_id {
            return Ok(cid.clone());
        }

        let chat = self
            .app_state
            .chat_service
            .create_chat(
                &task.user_id,
                CreateChatRequest {
                    space_id: task.space_id.clone(),
                    task_id: Some(task.id.clone()),
                    agent_id: task.agent_id.clone(),
                    title: Some(format!("Task: {}", task.title)),
                },
            )
            .await?;
        task.chat_id = Some(chat.id.clone());
        Ok(chat.id)
    }

    pub async fn save_initial_message_if_needed(
        &self,
        task: &Task,
        chat_id: &str,
    ) -> Result<(), AppError> {
        let stored_messages = self.app_state.chat_service.get_stored_messages(chat_id).await;
        if !stored_messages.is_empty() {
            return Ok(());
        }

        let source_agent_id = match &task.kind {
            TaskKind::Delegation { source_agent_id, .. } => source_agent_id.as_str(),
            _ => &task.agent_id,
        };
        self.app_state
            .chat_service
            .save_agent_message(chat_id, source_agent_id, task.description.clone())
            .await?;
        Ok(())
    }

    pub async fn handle_completed(
        &self,
        task: &Task,
        chat_id: &str,
        accumulated_text: String,
        last_segment: String,
        attachments: Vec<Attachment>,
    ) -> Result<(), AppError> {
        tracing::debug!(
            task_id = %task.id,
            chat_id = %chat_id,
            accumulated_text_len = accumulated_text.len(),
            last_segment_len = last_segment.len(),
            attachments_count = attachments.len(),
            "handle_completed: start"
        );

        if !accumulated_text.is_empty() {
            match self
                .app_state
                .chat_service
                .save_assistant_message_with_tool_calls(
                    chat_id, accumulated_text.clone(), None, attachments,
                )
                .await
            {
                Ok(_) => tracing::debug!(task_id = %task.id, "handle_completed: saved assistant message"),
                Err(e) => tracing::error!(task_id = %task.id, error = %e, "handle_completed: failed to save assistant message"),
            }
        } else {
            tracing::warn!(task_id = %task.id, "handle_completed: accumulated_text is empty, skipping message save");
        }

        let children = self
            .app_state
            .task_service
            .find_by_source_chat_id(chat_id)
            .await
            .unwrap_or_default();

        let has_incomplete_children = children
            .iter()
            .any(|c| matches!(c.status, TaskStatus::Pending | TaskStatus::InProgress));

        if has_incomplete_children {
            tracing::info!(
                task_id = %task.id,
                incomplete_children = children.iter().filter(|c| matches!(c.status, TaskStatus::Pending | TaskStatus::InProgress)).count(),
                "Task has incomplete children, staying InProgress"
            );
            return Ok(());
        }

        let result_text = if last_segment.is_empty() { &accumulated_text } else { &last_segment };
        let summary = if result_text.is_empty() {
            None
        } else {
            Some(result_text.to_string())
        };

        tracing::debug!(task_id = %task.id, has_summary = summary.is_some(), "handle_completed: marking task completed");

        self.app_state
            .task_service
            .mark_completed(&task.id, summary.clone())
            .await?;

        self.deliver_to_source(task, TaskStatus::Completed, summary.clone().unwrap_or_default())
            .await;

        self.broadcast_task_status(task, "completed", summary.as_deref());
        tracing::debug!(task_id = %task.id, "handle_completed: done");
        Ok(())
    }

    pub async fn handle_cancelled(
        &self,
        task: &Task,
        chat_id: &str,
        accumulated_text: String,
    ) -> Result<(), AppError> {
        if !accumulated_text.is_empty() {
            let _ = self
                .app_state
                .chat_service
                .save_assistant_message(chat_id, accumulated_text)
                .await;
        }

        self.app_state.task_service.mark_cancelled(&task.id).await?;
        self.broadcast_task_status(task, "cancelled", None);
        Ok(())
    }

    pub async fn handle_error(&self, task: &Task, error: &AppError) -> Result<(), AppError> {
        let error_msg = format!("Task execution error: {}", error);
        tracing::error!(error = %error, task_id = %task.id, "Task execution failed");
        self.app_state
            .task_service
            .mark_failed(&task.id, error_msg)
            .await?;
        self.deliver_to_source(task, TaskStatus::Failed, error.to_string())
            .await;
        self.broadcast_task_status(task, "failed", None);
        Ok(())
    }

    pub async fn deliver_to_source(&self, task: &Task, status: TaskStatus, text: String) {
        let TaskKind::Delegation {
            ref source_chat_id,
            deliver_directly,
            ..
        } = task.kind
        else {
            return;
        };

        let attachments = if status == TaskStatus::Completed {
            task.chat_id
                .as_ref()
                .map(|cid| {
                    self.app_state
                        .chat_service
                        .find_attachments_by_chat_id(cid)
                })
        } else {
            None
        };

        let attachments = match attachments {
            Some(fut) => fut.await.unwrap_or_default(),
            None => vec![],
        };

        match self
            .app_state
            .chat_service
            .save_task_completion_message(
                source_chat_id,
                &task.agent_id,
                text,
                MessageTool::TaskCompletion {
                    task_id: task.id.clone(),
                    chat_id: task.chat_id.clone(),
                    status,
                },
                attachments,
            )
            .await
        {
            Ok(msg) => {
                self.app_state.broadcast_service.broadcast_chat_message(
                    &task.user_id,
                    source_chat_id,
                    msg,
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, task_id = %task.id, "Failed to deliver task result to source chat");
            }
        }

        if !deliver_directly {
            self.check_and_resume_parent(source_chat_id, &task.user_id)
                .await;
        }
    }

    async fn check_and_resume_parent(&self, source_chat_id: &str, user_id: &str) {
        let siblings = match self
            .app_state
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

        let state = self.app_state.clone();
        let user_id = user_id.to_string();
        let chat_id = source_chat_id.to_string();
        tokio::spawn(async move {
            if let Err(e) =
                crate::api::routes::messages::resume_tool_loop(&state, &user_id, &chat_id)
                    .await
            {
                tracing::error!(error = %e, chat_id = %chat_id, "Failed to resume parent tool loop");
            }
        });
    }

    pub fn broadcast_task_status(&self, task: &Task, status: &str, summary: Option<&str>) {
        self.app_state.broadcast_service.broadcast_task_update(
            &task.user_id,
            &task.id,
            status,
            &task.title,
            task.chat_id.as_deref(),
            task.kind.source_chat_id(),
            summary,
        );
    }
}
