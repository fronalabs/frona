use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::task::models::{SignalMode, Task, TaskKind, TaskStatus};
use crate::chat::message::models::{MessageEvent, MessageRole};
use crate::chat::models::CreateChatRequest;
use crate::core::error::AppError;
use crate::core::execution::{ExecutionKind, ExecutionSource, ExecutionSourceKind, NewExecution};
use crate::inference::InferenceResponse;
use crate::inference::conversation::TaskConversationBuilder;
use crate::inference::tool_call::TaskEvent;
use crate::storage::Attachment;

const MAX_TASK_RETRIES: usize = 10;

const QUARANTINED_TASK_TOOLS: &[&str] = &["complete_task", "fail_task", "defer_task"];
const QUARANTINED_CONTINUOUS_SIGNAL_TOOLS: &[&str] =
    &["report_signal", "complete_task", "fail_task"];

fn quarantine_filter(task: &Task) -> Option<crate::tool::registry::ToolFilter> {
    use crate::tool::registry::ToolFilter;
    if !task.quarantined {
        return None;
    }
    if let TaskKind::Signal {
        mode: SignalMode::Continuous,
        ..
    } = task.kind
    {
        return Some(ToolFilter::AllowList(QUARANTINED_CONTINUOUS_SIGNAL_TOOLS));
    }
    Some(ToolFilter::AllowList(QUARANTINED_TASK_TOOLS))
}

/// Currently quarantine-only. The universal task-execution deny lives in
/// `ChatSessionContext::build` so every entry point gets it.
fn tool_filters_for_task(task: &Task) -> Vec<crate::tool::registry::ToolFilter> {
    quarantine_filter(task).into_iter().collect()
}

pub enum TaskLifecycleEvent {
    Completion {
        status: TaskStatus,
        summary: Option<String>,
    },
    /// Non-terminal. Do NOT resume parent.
    Match {
        attempt_index: u32,
        summary: String,
        result: Option<serde_json::Value>,
    },
}

fn source_chat_id_for(task: &Task) -> Option<&str> {
    match &task.kind {
        TaskKind::Delegation { source_chat_id, .. } => Some(source_chat_id.as_str()),
        TaskKind::Direct {
            source_chat_id: Some(source_chat_id),
        } => Some(source_chat_id.as_str()),
        TaskKind::Signal { source_chat_id, .. } => Some(source_chat_id.as_str()),
        TaskKind::CronRun {
            source_chat_id: Some(source_chat_id),
            ..
        } => Some(source_chat_id.as_str()),
        _ => None,
    }
}

fn source_chat_id_and_resume(task: &Task) -> Option<(&str, bool)> {
    match &task.kind {
        TaskKind::Delegation {
            source_chat_id,
            resume_parent,
            ..
        } => Some((source_chat_id.as_str(), *resume_parent)),
        TaskKind::Direct {
            source_chat_id: Some(source_chat_id),
        } => Some((source_chat_id.as_str(), false)),
        TaskKind::Signal {
            source_chat_id,
            resume_parent,
            ..
        } => Some((source_chat_id.as_str(), *resume_parent)),
        // CronRun resolves its resume flag against the template (async), so
        // resume_parent_if_requested handles it; skip this sync path.
        TaskKind::CronRun { .. } => None,
        _ => None,
    }
}

/// Returns `(content, schema)` to persist on the completion message, or
/// `None` to silently skip delivery (parsed value is null / empty obj/arr).
/// `schema` is `Some` only when `content` is JSON — that's the signal for
/// renderers (LLM, adapters, UI) to re-render. `Failed` and no-schema cases
/// pass `summary` through as plain prose.
fn build_completion_body(
    task: &Task,
    status: &TaskStatus,
    summary: Option<&str>,
) -> Option<(String, Option<serde_json::Value>)> {
    let legacy = || summary.unwrap_or("").to_string();
    if !matches!(status, TaskStatus::Completed) {
        return Some((legacy(), None));
    }
    let Some(schema) = task.effective_result_schema() else {
        return Some((legacy(), None));
    };
    let summary_str = summary.unwrap_or("");
    let spec = match crate::agent::task::schema::ResultSpec::new(schema.clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(task_id = %task.id, error = %e, "task.result_schema is invalid; using raw summary");
            return Some((legacy(), None));
        }
    };
    let value = match spec.parse(summary_str) {
        Ok(v) => v,
        Err(_) => return Some((legacy(), None)),
    };
    if value.is_null() {
        return None;
    }
    if let Some(obj) = value.as_object()
        && obj.is_empty()
    {
        return None;
    }
    if let Some(arr) = value.as_array()
        && arr.is_empty()
    {
        return None;
    }
    let json = serde_json::to_string(&value).unwrap_or_default();
    Some((json, Some(schema)))
}

fn build_message_event(task: &Task, event: TaskLifecycleEvent) -> Option<(String, MessageEvent)> {
    match event {
        TaskLifecycleEvent::Completion { status, summary } => {
            let (content, schema) = build_completion_body(task, &status, summary.as_deref())?;
            let evt = MessageEvent::TaskCompletion {
                task_id: task.id.clone(),
                chat_id: task.chat_id.clone(),
                status,
                summary: if content.is_empty() {
                    None
                } else {
                    Some(content.clone())
                },
                schema,
            };
            Some((content, evt))
        }
        TaskLifecycleEvent::Match {
            attempt_index,
            summary,
            result,
        } => {
            let content = summary.clone();
            let evt = MessageEvent::TaskMatch {
                task_id: task.id.clone(),
                chat_id: task.chat_id.clone(),
                attempt_index,
                summary,
                result,
            };
            Some((content, evt))
        }
    }
}

pub async fn deliver_event_to_source(
    chat_service: &crate::chat::service::ChatService,
    task: &Task,
    event: TaskLifecycleEvent,
    attachments: Vec<Attachment>,
) {
    let Some(source_chat_id) = source_chat_id_for(task) else {
        return;
    };

    let Some((content, message_event)) = build_message_event(task, event) else {
        tracing::debug!(task_id = %task.id, "schema rendered to silent — skipping source-chat delivery");
        return;
    };

    if let Err(e) = chat_service
        .save_task_lifecycle_message(
            &task.user_id,
            task.space_id.as_deref(),
            source_chat_id,
            &task.agent_id,
            content,
            message_event,
            attachments,
        )
        .await
    {
        tracing::warn!(error = %e, task_id = %task.id, "Failed to deliver task result to source chat");
    }
}

pub struct TaskExecutor {
    harness: Arc<crate::agent::harness::Harness>,
    active_tasks: Arc<Mutex<HashMap<String, CancellationToken>>>,
    max_concurrent_tasks: usize,
}

/// Removes the `active_tasks` entry on drop so a panic in `execute_task`
/// doesn't leak the slot. The actual `remove` runs in a `tokio::spawn` because
/// `Mutex` is async.
struct ActiveGuard {
    map: Arc<Mutex<HashMap<String, CancellationToken>>>,
    key: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let map = self.map.clone();
        let key = std::mem::take(&mut self.key);
        tokio::spawn(async move {
            map.lock().await.remove(&key);
        });
    }
}

impl TaskExecutor {
    pub fn new(harness: Arc<crate::agent::harness::Harness>) -> Self {
        let max = harness.config.server.max_concurrent_tasks;
        Self::with_max_concurrent_tasks(max, harness)
    }

    pub fn with_max_concurrent_tasks(
        max_concurrent_tasks: usize,
        harness: Arc<crate::agent::harness::Harness>,
    ) -> Self {
        Self {
            harness,
            active_tasks: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent_tasks,
        }
    }

    pub async fn resume_all(self: &Arc<Self>) {
        // Crash-orphan sweep MUST run before spawning resumable tasks: otherwise
        // the freshly-spawned InProgress CronRuns would themselves be matched
        // by the orphan query and flipped to Failed mid-turn, yanking
        // `complete_task` out of the agent's registry on the next session build.
        self.resume_in_flight_crons().await;

        let tasks = match self.harness.task_service.find_resumable().await {
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

            let executor = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(e) = executor.run_task(task).await {
                    tracing::warn!(error = %e, "Task failed during resume");
                }
            });
        }
    }

    /// Marks crash-interrupted CronRuns Failed instead of restarting them.
    /// The next scheduled tick fires fresh if the cron's concurrency allows.
    async fn resume_in_flight_crons(&self) {
        let orphans = match self.harness.task_service.find_orphaned_cron_runs().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "Failed to query orphaned CronRuns");
                return;
            }
        };

        if orphans.is_empty() {
            return;
        }

        tracing::info!(count = orphans.len(), "Marking orphaned CronRuns as Failed");

        for run in orphans {
            if let Err(e) = self
                .harness
                .task_service
                .mark_failed(
                    &run.id,
                    "Server restarted while CronRun was in flight".to_string(),
                )
                .await
            {
                tracing::warn!(error = %e, task_id = %run.id, "Failed to mark orphan CronRun");
            }
        }
    }

    pub async fn run_task_by_id(&self, task_id: &str) -> Result<(), AppError> {
        let task = self
            .harness
            .task_service
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("task {task_id}")))?;
        self.run_task(task).await
    }

    pub async fn run_task(&self, task: Task) -> Result<(), AppError> {
        if self.harness.shutdown_token.is_cancelled() {
            tracing::info!(task_id = %task.id, "Rejecting task during shutdown");
            return Ok(());
        }

        let agent_max = self.get_agent_concurrent_limit(&task.agent_id).await;
        let key = format!("{}:{}", task.agent_id, task.id);
        let cancel_token = CancellationToken::new();

        {
            let mut active = self.active_tasks.lock().await;

            if active.contains_key(&key) {
                tracing::info!(task_id = %task.id, "Task already running; skipping duplicate run");
                return Ok(());
            }

            if active.len() >= self.max_concurrent_tasks {
                tracing::info!(
                    task_id = %task.id,
                    active = active.len(),
                    limit = self.max_concurrent_tasks,
                    "Global concurrency limit reached, task stays Pending"
                );
                return Ok(());
            }

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

            active.insert(key.clone(), cancel_token.clone());
        }

        let _guard = ActiveGuard {
            map: self.active_tasks.clone(),
            key,
        };
        let (kind, source) = match &task.kind {
            TaskKind::CronRun { source_cron_id, .. } => (
                ExecutionKind::Scheduled,
                ExecutionSource {
                    kind: ExecutionSourceKind::Schedule,
                    id: Some(source_cron_id.clone()),
                },
            ),
            _ => (
                ExecutionKind::Task,
                ExecutionSource {
                    kind: ExecutionSourceKind::Task,
                    id: Some(task.id.clone()),
                },
            ),
        };
        let _execution = self.harness.execution_registry.start(
            &task.user_id,
            NewExecution {
                title: task.title.clone(),
                kind,
                action: Some("Running task".to_string()),
                source: Some(source),
                related_chat_ids: task
                    .kind
                    .source_chat_id()
                    .map(|chat_id| vec![chat_id.to_string()])
                    .unwrap_or_default(),
                // Cancelling the source schedule would also disable future runs.
                can_cancel: !matches!(&task.kind, TaskKind::CronRun { .. }),
            },
        );
        self.execute_task(task, cancel_token).await
    }

    pub async fn cancel_task(&self, task_id: &str) -> bool {
        // Always persist Cancelled so paused tasks (which have no entry in
        // `active_tasks`) still get cancelled. For live tasks `handle_cancelled`
        // will write the same status again — idempotent.
        if let Err(e) = self.harness.task_service.mark_cancelled(task_id).await {
            tracing::warn!(error = %e, task_id = %task_id, "Failed to persist task cancellation");
        }

        let direct = {
            let active = self.active_tasks.lock().await;
            let mut hit = false;
            for (key, token) in active.iter() {
                if key.ends_with(&format!(":{}", task_id)) {
                    token.cancel();
                    hit = true;
                    break;
                }
            }
            hit
        };

        // Cron templates aren't in active_tasks themselves; only their CronRun
        // children are. Each cancelled run's tokio::spawn cleans itself up.
        if let Ok(Some(task)) = self.harness.task_service.find_by_id(task_id).await
            && matches!(task.kind, TaskKind::Cron { .. })
            && let Ok(active_runs) = self
                .harness
                .task_service
                .find_active_runs_by_cron(task_id)
                .await
        {
            let active = self.active_tasks.lock().await;
            for run in active_runs {
                let suffix = format!(":{}", run.id);
                for (key, token) in active.iter() {
                    if key.ends_with(&suffix) {
                        token.cancel();
                        break;
                    }
                }
            }
        }

        direct
    }

    pub async fn register_cancellation(
        &self,
        agent_id: &str,
        task_id: &str,
        token: CancellationToken,
    ) {
        let key = format!("{}:{}", agent_id, task_id);
        self.active_tasks.lock().await.insert(key, token);
    }

    pub async fn unregister_cancellation(&self, agent_id: &str, task_id: &str) {
        let key = format!("{}:{}", agent_id, task_id);
        self.active_tasks.lock().await.remove(&key);
    }

    async fn get_agent_concurrent_limit(&self, agent_id: &str) -> usize {
        if let Ok(Some(agent)) = self.harness.agent_service.find_by_id(agent_id).await {
            return agent.max_concurrent_tasks.unwrap_or(3) as usize;
        }
        3
    }

    async fn execute_task(
        &self,
        mut task: Task,
        cancel_token: CancellationToken,
    ) -> Result<(), AppError> {
        let task_id = task.id.clone();

        let current_status = self
            .harness
            .task_service
            .find_by_id(&task_id)
            .await?
            .map(|t| t.status);

        if matches!(current_status, Some(TaskStatus::Cancelled)) {
            tracing::info!(task_id = %task_id, "Task already cancelled, skipping");
            return Ok(());
        }

        let chat_id = self.ensure_task_chat(&mut task).await?;

        self.harness
            .task_service
            .mark_in_progress(&task_id, Some(&chat_id))
            .await?;

        self.save_initial_message_if_needed(&task, &chat_id).await?;

        for turn in 0..MAX_TASK_RETRIES {
            // Check for unprocessed lifecycle events (crash recovery)
            if let Some(action) = self.find_lifecycle_event(&chat_id).await {
                self.handle_lifecycle_action(&task, &chat_id, action)
                    .await?;
                return Ok(());
            }

            let continuation_prompt = if turn > 0 {
                self.harness.prompts.read("TASK_CONTINUATION.md")
            } else {
                None
            };

            let agent_msg_id = match self
                .harness
                .chat_service
                .find_executing_message_for_chat(&chat_id)
                .await
            {
                Ok(Some(msg)) => msg.id,
                _ => {
                    let msg = self
                        .harness
                        .chat_service
                        .create_executing_agent_message(&chat_id, &task.agent_id)
                        .await?;
                    msg.id
                }
            };

            let session_token = self.harness.active_sessions.register(&chat_id).await;
            let builder = Box::new(TaskConversationBuilder {
                user_service: self.harness.user_service.clone(),
                storage_service: self.harness.storage_service.clone(),
                agent_service: self.harness.agent_service.clone(),
                continuation_prompt: continuation_prompt.clone(),
            });
            let filters = tool_filters_for_task(&task);
            let result = self
                .harness
                .run_loop(
                    &task.user_id,
                    &chat_id,
                    &agent_msg_id,
                    cancel_token.clone(),
                    builder,
                    &filters,
                    None,
                )
                .await;
            drop(session_token);
            self.harness.active_sessions.remove(&chat_id).await;

            match result {
                Ok(crate::agent::harness::AgentLoopOutcome {
                    inference,
                    mut response,
                }) => match inference {
                    InferenceResponse::Completed {
                        text,
                        attachments,
                        lifecycle_event,
                        reasoning,
                        ..
                    } => {
                        response.content = text;
                        response.attachments = attachments;
                        response.reasoning = reasoning;
                        let _ = self
                            .harness
                            .chat_service
                            .complete_agent_message(response)
                            .await;

                        if let Some(event) = lifecycle_event {
                            let action = self.lifecycle_action_from_event(event);
                            self.handle_lifecycle_action(&task, &chat_id, action)
                                .await?;
                            return Ok(());
                        }

                        if let Some(action) = self.find_lifecycle_event(&chat_id).await {
                            self.handle_lifecycle_action(&task, &chat_id, action)
                                .await?;
                            return Ok(());
                        }
                        continue;
                    }
                    InferenceResponse::ExternalToolPending { tool_calls, .. } => {
                        let _ = self
                            .harness
                            .chat_service
                            .pause_agent_message(
                                response,
                                crate::inference::tool_loop::PauseReason::Hitl,
                                tool_calls,
                            )
                            .await;
                        // Exit cleanly. The HITL-resolve handler will respawn
                        // this task via `run_task` once the human resolves.
                        return Ok(());
                    }
                    InferenceResponse::Cancelled(text) => {
                        response.content = text;
                        let _ = self
                            .harness
                            .chat_service
                            .cancel_agent_message(response)
                            .await;
                        self.handle_cancelled(&task).await?;
                        return Ok(());
                    }
                    InferenceResponse::Handled => {
                        // Command dispatch already finalized the response.
                        // Treat as a no-op turn end — let the task continue or wrap up.
                        return Ok(());
                    }
                },
                Err(e) => {
                    if let Ok(msg) = self
                        .harness
                        .chat_service
                        .get_message(&task.user_id, &agent_msg_id)
                        .await
                    {
                        let _ = self
                            .harness
                            .chat_service
                            .fail_agent_message(msg, e.to_string())
                            .await;
                    }
                    self.handle_error(&task, &e).await?;
                    return Ok(());
                }
            }
        }

        tracing::warn!(
            task_id = %task.id,
            turns = MAX_TASK_RETRIES,
            "Task reached max retries, auto-completing"
        );
        self.harness
            .task_service
            .mark_completed(
                &task.id,
                Some("Task auto-completed after max retries".into()),
            )
            .await?;
        deliver_event_to_source(
            &self.harness.chat_service,
            &task,
            TaskLifecycleEvent::Completion {
                status: TaskStatus::Completed,
                summary: Some("Task auto-completed after max retries".into()),
            },
            vec![],
        )
        .await;
        self.resume_parent_if_requested(&task).await;

        Ok(())
    }

    async fn find_lifecycle_event(&self, chat_id: &str) -> Option<LifecycleAction> {
        let messages = match self.harness.chat_service.get_stored_messages(chat_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(chat_id, error = %e, "find_lifecycle_event: failed to load stored messages");
                return None;
            }
        };

        let deliverables = {
            let tool_calls = match self.harness.chat_service.get_tool_calls(chat_id).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(chat_id, error = %e, "find_lifecycle_event: failed to load tool calls");
                    return None;
                }
            };
            tool_calls
                .into_iter()
                .rev()
                .find_map(|te| match te.task_event {
                    Some(TaskEvent::Completion { deliverables, .. })
                        if !deliverables.is_empty() =>
                    {
                        Some(deliverables)
                    }
                    _ => None,
                })
                .unwrap_or_default()
        };

        for msg in messages.iter().rev() {
            if msg.role != MessageRole::System {
                continue;
            }
            match &msg.event {
                Some(MessageEvent::TaskCompletion {
                    status, summary, ..
                }) => {
                    return Some(LifecycleAction::Complete {
                        status: status.clone(),
                        summary: summary.clone(),
                        attachments: deliverables,
                    });
                }
                Some(MessageEvent::TaskDeferred {
                    delay_minutes,
                    reason,
                    ..
                }) => {
                    return Some(LifecycleAction::Defer {
                        delay_minutes: *delay_minutes,
                        reason: reason.clone(),
                    });
                }
                _ => continue,
            }
        }
        None
    }

    fn lifecycle_action_from_event(&self, event: TaskEvent) -> LifecycleAction {
        match event {
            TaskEvent::Completion {
                status,
                summary,
                deliverables,
                ..
            } => LifecycleAction::Complete {
                status,
                summary,
                attachments: deliverables,
            },
            TaskEvent::Deferred {
                delay_minutes,
                reason,
                ..
            } => LifecycleAction::Defer {
                delay_minutes,
                reason,
            },
        }
    }

    async fn handle_lifecycle_action(
        &self,
        task: &Task,
        _chat_id: &str,
        action: LifecycleAction,
    ) -> Result<(), AppError> {
        match action {
            LifecycleAction::Complete {
                status: TaskStatus::Completed,
                summary,
                attachments,
            } => {
                self.harness
                    .task_service
                    .mark_completed(&task.id, summary.clone())
                    .await?;
                deliver_event_to_source(
                    &self.harness.chat_service,
                    task,
                    TaskLifecycleEvent::Completion {
                        status: TaskStatus::Completed,
                        summary: summary.clone(),
                    },
                    attachments,
                )
                .await;
                self.resume_parent_if_requested(task).await;
            }
            LifecycleAction::Complete {
                status: TaskStatus::Failed,
                summary,
                attachments,
            } => {
                let error_msg = summary.clone().unwrap_or_default();
                self.harness
                    .task_service
                    .mark_failed(&task.id, error_msg)
                    .await?;
                deliver_event_to_source(
                    &self.harness.chat_service,
                    task,
                    TaskLifecycleEvent::Completion {
                        status: TaskStatus::Failed,
                        summary,
                    },
                    attachments,
                )
                .await;
                self.resume_parent_if_requested(task).await;
            }
            LifecycleAction::Complete { .. } => {}
            LifecycleAction::Defer {
                delay_minutes,
                reason,
            } => {
                let run_at = Utc::now() + chrono::Duration::minutes(delay_minutes as i64);
                self.harness
                    .task_service
                    .mark_deferred(&task.id, run_at, &reason)
                    .await?;
            }
        }
        Ok(())
    }

    /// Lifecycle events emitted by the turn (`complete_task` / `fail_task`)
    /// resume the parent via `deliver_to_source`.
    pub async fn run_with_injected_message(
        &self,
        task: &Task,
        system_message: String,
    ) -> Result<(), AppError> {
        let mut task_for_chat = task.clone();
        let was_unset = task_for_chat.chat_id.is_none();
        let chat_id = self.ensure_task_chat(&mut task_for_chat).await?;
        if was_unset {
            // ensure_task_chat populated chat_id on the local clone; persist
            // it so subsequent calls reuse C₂ instead of creating a new chat.
            self.harness.task_service.save(&task_for_chat).await?;
        }

        self.harness
            .chat_service
            .save_system_message(
                &task.user_id,
                task.space_id.as_deref(),
                &chat_id,
                system_message,
            )
            .await?;

        let agent_msg = self
            .harness
            .chat_service
            .create_executing_agent_message(&chat_id, &task.agent_id)
            .await?;
        let agent_msg_id = agent_msg.id.clone();
        let cancel_token = CancellationToken::new();

        let builder = Box::new(TaskConversationBuilder {
            user_service: self.harness.user_service.clone(),
            storage_service: self.harness.storage_service.clone(),
            agent_service: self.harness.agent_service.clone(),
            continuation_prompt: None,
        });
        let filters = tool_filters_for_task(task);
        let outcome = self
            .harness
            .run_loop(
                &task.user_id,
                &chat_id,
                &agent_msg_id,
                cancel_token,
                builder,
                &filters,
                None,
            )
            .await;

        // Signal tasks complete via tool call, not System MessageEvent.
        let mut lifecycle_event = None;
        match outcome {
            Ok(crate::agent::harness::AgentLoopOutcome {
                inference,
                mut response,
            }) => {
                if let InferenceResponse::Completed {
                    text,
                    attachments,
                    reasoning,
                    lifecycle_event: lc,
                    ..
                } = inference
                {
                    response.content = text;
                    response.attachments = attachments;
                    response.reasoning = reasoning;
                    let _ = self
                        .harness
                        .chat_service
                        .complete_agent_message(response)
                        .await;
                    lifecycle_event = lc;
                }
            }
            Err(e) => {
                if let Ok(msg) = self
                    .harness
                    .chat_service
                    .get_message(&task.user_id, &agent_msg_id)
                    .await
                {
                    let _ = self
                        .harness
                        .chat_service
                        .fail_agent_message(msg, e.to_string())
                        .await;
                }
                tracing::warn!(
                    task_id = %task.id,
                    error = %e,
                    "Agent failed during injected-message run"
                );
                return Err(e);
            }
        }

        let action = if let Some(event) = lifecycle_event {
            Some(self.lifecycle_action_from_event(event))
        } else {
            self.find_lifecycle_event(&chat_id).await
        };

        if let Some(action) = action
            && let Err(e) = self.handle_lifecycle_action(task, &chat_id, action).await
        {
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "Failed to apply lifecycle action after injected-message run"
            );
        }

        Ok(())
    }

    pub async fn ensure_task_chat(&self, task: &mut Task) -> Result<String, AppError> {
        if let Some(ref cid) = task.chat_id {
            return Ok(cid.clone());
        }

        let chat = self
            .harness
            .chat_service
            .create_chat(
                &task.user_id,
                CreateChatRequest {
                    space_id: task.space_id.clone(),
                    task_id: Some(task.id.clone()),
                    agent_id: task.agent_id.clone(),
                    title: Some(format!("Task: {}", task.title)),
                    metadata: None,
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
        let stored_messages = self
            .harness
            .chat_service
            .get_stored_messages(chat_id)
            .await?;
        if !stored_messages.is_empty() {
            return Ok(());
        }

        let source_agent_id = match &task.kind {
            TaskKind::Delegation {
                source_agent_id, ..
            } => source_agent_id.as_str(),
            _ => &task.agent_id,
        };
        self.harness
            .chat_service
            .save_agent_message(
                &task.user_id,
                task.space_id.as_deref(),
                chat_id,
                source_agent_id,
                task.description.clone(),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn handle_cancelled(&self, task: &Task) -> Result<(), AppError> {
        self.harness.task_service.mark_cancelled(&task.id).await?;
        Ok(())
    }

    pub async fn handle_error(&self, task: &Task, error: &AppError) -> Result<(), AppError> {
        let error_msg = format!("Task execution error: {}", error);
        tracing::error!(error = %error, task_id = %task.id, "Task execution failed");
        self.harness
            .task_service
            .mark_failed(&task.id, error_msg)
            .await?;
        deliver_event_to_source(
            &self.harness.chat_service,
            task,
            TaskLifecycleEvent::Completion {
                status: TaskStatus::Failed,
                summary: Some(error.to_string()),
            },
            vec![],
        )
        .await;
        self.resume_parent_if_requested(task).await;
        Ok(())
    }

    /// Terminal-only. Match would spawn concurrent loops.
    pub async fn resume_parent_if_requested(&self, task: &Task) {
        if let TaskKind::CronRun {
            source_cron_id,
            source_chat_id: Some(chat_id),
            ..
        } = &task.kind
        {
            let template = match self.harness.task_service.find_by_id(source_cron_id).await {
                Ok(Some(t)) => t,
                _ => return,
            };
            let process_result = matches!(
                template.kind,
                TaskKind::Cron {
                    process_result: true,
                    ..
                }
            );
            if !process_result {
                return;
            }
            self.check_and_resume_parent(chat_id.as_str(), &task.user_id)
                .await;
            return;
        }

        let Some((source_chat_id, true)) = source_chat_id_and_resume(task) else {
            return;
        };
        self.check_and_resume_parent(source_chat_id, &task.user_id)
            .await;
    }

    async fn check_and_resume_parent(&self, source_chat_id: &str, user_id: &str) {
        // The flag opt-in is enforced by the caller, so user chats and task
        // chats are both eligible — that's the difference from the prior gate.

        let siblings = match self
            .harness
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
            "All child tasks complete, resuming parent"
        );

        let chat = match self.harness.chat_service.find_chat(source_chat_id).await {
            Ok(Some(chat)) => chat,
            Ok(None) => {
                tracing::warn!(chat_id = %source_chat_id, "Source chat not found; cannot resume");
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, chat_id = %source_chat_id, "Failed to lookup source chat");
                return;
            }
        };

        if let Some(parent_task_id) = chat.task_id.as_ref() {
            // Box::pin breaks the async recursion cycle (run_task_by_id →
            // execute_task → handle_lifecycle_action → ... → run_task_by_id).
            if let Err(e) = Box::pin(self.run_task_by_id(parent_task_id)).await {
                tracing::error!(error = %e, chat_id = %source_chat_id, "Failed to run parent task for resume");
            }
            return;
        }

        let existing = match self
            .harness
            .chat_service
            .find_executing_message_for_chat(source_chat_id)
            .await
        {
            Ok(msg) => msg,
            Err(e) => {
                tracing::error!(error = %e, chat_id = %source_chat_id, "Failed to find executing message");
                return;
            }
        };
        let message_id = if let Some(msg) = existing {
            msg.id
        } else {
            // User chats settle their assistant turn before tasks complete;
            // mint a fresh executing message so the loop has a write target.
            match self
                .harness
                .chat_service
                .create_executing_agent_message(source_chat_id, &chat.agent_id)
                .await
            {
                Ok(msg) => msg.id,
                Err(e) => {
                    tracing::error!(error = %e, chat_id = %source_chat_id, "Failed to create executing message for resume");
                    return;
                }
            }
        };
        if let Err(e) = self
            .harness
            .resume(user_id, source_chat_id, &message_id)
            .await
        {
            tracing::error!(error = %e, chat_id = %source_chat_id, "Failed to resume user chat");
        }
    }
}

enum LifecycleAction {
    Complete {
        status: TaskStatus,
        summary: Option<String>,
        attachments: Vec<Attachment>,
    },
    Defer {
        delay_minutes: u32,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn signal_task(quarantined: bool, mode: SignalMode) -> Task {
        Task {
            id: "t".into(),
            user_id: "u".into(),
            agent_id: "a".into(),
            space_id: None,
            chat_id: Some("task-chat".into()),
            title: "x".into(),
            description: "y".into(),
            status: TaskStatus::Pending,
            kind: TaskKind::Signal {
                source_chat_id: "src".into(),
                resume_parent: true,
                mode,
                expected_categories: vec!["t".into()],
                expected_channels: vec![],
                expected_contacts: vec![],
                expires_at: None,
                max_evaluations: 50,
                evaluation_count: 0,
            },
            run_at: None,
            result_summary: None,
            error_message: None,
            quarantined,
            result_schema: None,
            result_description: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn direct_task(quarantined: bool) -> Task {
        Task {
            id: "t".into(),
            user_id: "u".into(),
            agent_id: "a".into(),
            space_id: None,
            chat_id: None,
            title: "x".into(),
            description: "y".into(),
            status: TaskStatus::Pending,
            kind: TaskKind::Direct {
                source_chat_id: None,
            },
            run_at: None,
            result_summary: None,
            error_message: None,
            quarantined,
            result_schema: None,
            result_description: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn quarantine_filter_returns_none_when_not_quarantined() {
        assert!(quarantine_filter(&direct_task(false)).is_none());
        assert!(quarantine_filter(&signal_task(false, SignalMode::Once)).is_none());
        assert!(quarantine_filter(&signal_task(false, SignalMode::Continuous)).is_none());
    }

    #[test]
    fn quarantine_filter_default_for_once_signal_and_other_kinds() {
        use crate::tool::registry::ToolFilter;
        assert_eq!(
            quarantine_filter(&direct_task(true)),
            Some(ToolFilter::AllowList(QUARANTINED_TASK_TOOLS))
        );
        assert_eq!(
            quarantine_filter(&signal_task(true, SignalMode::Once)),
            Some(ToolFilter::AllowList(QUARANTINED_TASK_TOOLS))
        );
    }

    #[test]
    fn quarantine_filter_continuous_signal_swaps_in_report_signal() {
        use crate::tool::registry::ToolFilter;
        let filter = quarantine_filter(&signal_task(true, SignalMode::Continuous))
            .expect("continuous quarantined task gets a filter");
        let tools = match filter {
            ToolFilter::AllowList(t) => t,
            _ => panic!("expected AllowList"),
        };
        assert_eq!(tools, QUARANTINED_CONTINUOUS_SIGNAL_TOOLS);
        assert!(tools.contains(&"report_signal"));
        assert!(tools.contains(&"complete_task"));
        assert!(tools.contains(&"fail_task"));
        assert!(!tools.contains(&"defer_task"));
    }
}
