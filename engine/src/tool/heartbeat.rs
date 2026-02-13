use chrono::{Duration, Utc};
use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::agent::service::AgentService;
use crate::agent::workspace::AgentWorkspaceManager;
use crate::core::error::AppError;
use frona_derive::agent_tool;

use super::{ToolContext, ToolOutput};

pub struct HeartbeatTool {
    agent_service: AgentService,
    agent_workspaces: AgentWorkspaceManager,
    agent_id: String,
    prompts: PromptLoader,
}

impl HeartbeatTool {
    pub fn new(
        agent_service: AgentService,
        agent_workspaces: AgentWorkspaceManager,
        agent_id: String,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            agent_service,
            agent_workspaces,
            agent_id,
            prompts,
        }
    }
}

#[agent_tool(files("set_heartbeat"))]
impl HeartbeatTool {
    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let interval_minutes = arguments
            .get("interval_minutes")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| AppError::Validation("interval_minutes is required".into()))?;

        if interval_minutes > 0 {
            let ws = self.agent_workspaces.get(&self.agent_id);
            match ws.read("HEARTBEAT.md") {
                Some(content) if !content.trim().is_empty() => {}
                _ => {
                    return Err(AppError::Validation(
                        "HEARTBEAT.md is missing or empty. Write your heartbeat checklist to HEARTBEAT.md first.".into(),
                    ));
                }
            }
        }

        if interval_minutes == 0 {
            self.agent_service
                .set_heartbeat(&self.agent_id, None)
                .await?;

            return Ok(ToolOutput::text(serde_json::json!({
                "message": "Heartbeat disabled.",
                "heartbeat_interval": null,
                "next_heartbeat_at": null,
            }).to_string()));
        }

        let next = Utc::now() + Duration::minutes(interval_minutes as i64);
        self.agent_service
            .set_heartbeat(&self.agent_id, Some(interval_minutes))
            .await?;

        Ok(ToolOutput::text(serde_json::json!({
            "message": format!("Heartbeat set to every {} minutes. Next heartbeat at {}.", interval_minutes, next.format("%Y-%m-%d %H:%M UTC")),
            "heartbeat_interval": interval_minutes,
            "next_heartbeat_at": next.to_rfc3339(),
        }).to_string()))
    }
}
