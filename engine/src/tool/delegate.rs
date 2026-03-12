use std::sync::Arc;

use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::agent::service::AgentService;
use crate::agent::task::models::CreateTaskRequest;
use crate::agent::task::executor::TaskExecutor;
use crate::agent::task::service::TaskService;
use crate::chat::broadcast::BroadcastService;
use crate::core::error::AppError;
use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput};

pub struct DelegateTaskTool {
    task_service: TaskService,
    agent_service: AgentService,
    task_executor: Arc<TaskExecutor>,
    broadcast_service: BroadcastService,
    user_id: String,
    agent_id: String,
    chat_id: String,
    space_id: Option<String>,
    prompts: PromptLoader,
}

impl DelegateTaskTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        task_service: TaskService,
        agent_service: AgentService,
        task_executor: Arc<TaskExecutor>,
        broadcast_service: BroadcastService,
        user_id: String,
        agent_id: String,
        chat_id: String,
        space_id: Option<String>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            task_service,
            agent_service,
            task_executor,
            broadcast_service,
            user_id,
            agent_id,
            chat_id,
            space_id,
            prompts,
        }
    }
}

#[agent_tool(name = "delegate", files("delegate_task", "run_subtask"))]
impl DelegateTaskTool {
    async fn execute(&self, tool_name: &str, arguments: Value, _ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
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
            .agent_service
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

        self.broadcast_service.broadcast_task_update(
            &self.user_id,
            &task_id,
            "pending",
            &task_response.title,
            task_response.chat_id.as_deref(),
            Some(&self.chat_id),
            None,
        );

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
