use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::repository::AgentRepository;
use crate::agent::task::dto::CreateTaskRequest;
use crate::agent::task::executor::TaskExecutor;
use crate::agent::task::service::TaskService;
use crate::error::AppError;

use super::{AgentTool, ToolContext, ToolDefinition, ToolOutput};

pub struct DelegateTaskTool {
    task_service: TaskService,
    agent_repo: Arc<dyn AgentRepository>,
    task_executor: Arc<TaskExecutor>,
    user_id: String,
    agent_id: String,
    chat_id: String,
    space_id: Option<String>,
}

impl DelegateTaskTool {
    pub fn new(
        task_service: TaskService,
        agent_repo: Arc<dyn AgentRepository>,
        task_executor: Arc<TaskExecutor>,
        user_id: String,
        agent_id: String,
        chat_id: String,
        space_id: Option<String>,
    ) -> Self {
        Self {
            task_service,
            agent_repo,
            task_executor,
            user_id,
            agent_id,
            chat_id,
            space_id,
        }
    }
}

#[async_trait]
impl AgentTool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        let delegate_params = serde_json::json!({
            "type": "object",
            "properties": {
                "target_agent": {
                    "type": "string",
                    "description": "The name of the agent to delegate the task to (from <available_agents>)"
                },
                "title": {
                    "type": "string",
                    "description": "A short title for the task"
                },
                "instruction": {
                    "type": "string",
                    "description": "Detailed instructions for the target agent"
                },
                "run_at": {
                    "type": "string",
                    "description": "Optional ISO 8601 datetime to defer execution (e.g., '2026-03-15T14:00:00Z'). If omitted, the task runs immediately."
                }
            },
            "required": ["target_agent", "title", "instruction"]
        });

        vec![
            ToolDefinition {
                name: "delegate_task".to_string(),
                description: "Fire-and-forget: delegate a one-off task to another agent. The result \
                    is posted directly to this chat for the user — your tool loop is NOT resumed. \
                    Returns immediately with a task ID. Optionally set run_at to defer execution. \
                    For recurring scheduled work, use schedule_task. For periodic autonomous \
                    check-ins, use set_heartbeat. To get the result back and continue processing, \
                    use run_subtask instead.".to_string(),
                parameters: delegate_params.clone(),
            },
            ToolDefinition {
                name: "run_subtask".to_string(),
                description: "Run a subtask on another agent and resume when it completes. Unlike \
                    delegate_task, the result is returned to YOU (the calling agent) so you can \
                    process it further. Use this when you need the sub-agent's output to continue \
                    your work. Optionally set run_at to defer execution.".to_string(),
                parameters: delegate_params,
            },
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let deliver_directly = tool_name == "delegate_task";

        let target_agent_name = arguments
            .get("target_agent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'target_agent' parameter".into()))?;

        let title = arguments
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'title' parameter".into()))?;

        let instruction = arguments
            .get("instruction")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'instruction' parameter".into()))?;

        let target_agent = self
            .agent_repo
            .find_by_name(&self.user_id, target_agent_name)
            .await?
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "Agent '{}' not found. Check <available_agents> for valid agent names.",
                    target_agent_name
                ))
            })?;

        if !target_agent.enabled {
            return Err(AppError::Validation(format!(
                "Agent '{}' is disabled and cannot accept tasks.",
                target_agent.name
            )));
        }

        let run_at = arguments
            .get("run_at")
            .and_then(|v| v.as_str())
            .map(|s| s.parse::<chrono::DateTime<chrono::Utc>>())
            .transpose()
            .map_err(|e| AppError::Validation(format!("Invalid run_at datetime: {}", e)))?;

        let req = CreateTaskRequest {
            agent_id: target_agent.id.clone(),
            space_id: self.space_id.clone(),
            chat_id: None,
            title: title.to_string(),
            description: Some(instruction.to_string()),
            source_agent_id: Some(self.agent_id.clone()),
            source_chat_id: Some(self.chat_id.clone()),
            deliver_directly: Some(deliver_directly),
            run_at,
        };

        let task_response = self.task_service.create(&self.user_id, req).await?;
        let task_id = task_response.id.clone();

        if run_at.is_none() {
            let task = self
                .task_service
                .find_by_id(&task_id)
                .await?
                .ok_or_else(|| AppError::Internal("Task just created but not found".into()))?;

            if let Err(e) = self.task_executor.spawn_execution(task).await {
                tracing::warn!(error = %e, task_id = %task_id, "Failed to spawn task execution immediately");
            }
        }

        let message = match (run_at, deliver_directly) {
            (Some(at), _) => format!(
                "Task '{}' delegated to {}, deferred until {}.",
                title, target_agent.name, at.format("%Y-%m-%d %H:%M UTC")
            ),
            (None, true) => format!(
                "Task '{}' delegated to {}. Results will be posted to this chat when complete.",
                title, target_agent.name
            ),
            (None, false) => format!(
                "Subtask '{}' dispatched to {}. You will be resumed with the result.",
                title, target_agent.name
            ),
        };

        Ok(ToolOutput::text(serde_json::json!({
            "task_id": task_id,
            "target_agent": target_agent.name,
            "run_at": run_at.map(|t| t.to_rfc3339()),
            "message": message,
        }).to_string()))
    }
}

