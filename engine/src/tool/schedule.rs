use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::agent::repository::AgentRepository;
use crate::agent::task::service::TaskService;
use crate::error::AppError;

use super::{AgentTool, ToolContext, ToolDefinition, ToolOutput};

pub fn parse_cron(expression: &str) -> Result<cron::Schedule, AppError> {
    let seven_field = format!("0 {} *", expression);
    cron::Schedule::from_str(&seven_field)
        .map_err(|e| AppError::Validation(format!("Invalid cron expression '{}': {}", expression, e)))
}

pub fn next_cron_occurrence(expression: &str) -> Result<DateTime<Utc>, AppError> {
    let schedule = parse_cron(expression)?;
    schedule
        .upcoming(Utc)
        .next()
        .ok_or_else(|| AppError::Validation("Cron expression has no future occurrences".into()))
}

pub struct ScheduleTaskTool {
    task_service: TaskService,
    agent_repo: Arc<dyn AgentRepository>,
    user_id: String,
    agent_id: String,
    chat_id: String,
}

impl ScheduleTaskTool {
    pub fn new(
        task_service: TaskService,
        agent_repo: Arc<dyn AgentRepository>,
        user_id: String,
        agent_id: String,
        chat_id: String,
    ) -> Self {
        Self {
            task_service,
            agent_repo,
            user_id,
            agent_id,
            chat_id,
        }
    }

    async fn resolve_agent_id(&self, target_agent: Option<&str>) -> Result<String, AppError> {
        match target_agent {
            Some(name) => {
                let agent = self
                    .agent_repo
                    .find_by_name(&self.user_id, name)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation(format!(
                            "Agent '{}' not found. Check <available_agents> for valid agent names.",
                            name
                        ))
                    })?;
                if !agent.enabled {
                    return Err(AppError::Validation(format!(
                        "Agent '{}' is disabled.",
                        agent.name
                    )));
                }
                Ok(agent.id)
            }
            None => Ok(self.agent_id.clone()),
        }
    }

    async fn handle_create(&self, arguments: &Value) -> Result<ToolOutput, AppError> {
        let cron_expression = arguments
            .get("cron_expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'cron_expression' parameter".into()))?;

        let instruction = arguments
            .get("instruction")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'instruction' parameter".into()))?;

        let title = arguments
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(instruction);

        let target_agent = arguments.get("target_agent").and_then(|v| v.as_str());
        let agent_id = self.resolve_agent_id(target_agent).await?;

        let run_at = arguments
            .get("run_at")
            .and_then(|v| v.as_str())
            .map(|s| s.parse::<DateTime<Utc>>())
            .transpose()
            .map_err(|e| AppError::Validation(format!("Invalid run_at datetime: {}", e)))?;

        parse_cron(cron_expression)?;
        let next_run_at = match run_at {
            Some(dt) => dt,
            None => next_cron_occurrence(cron_expression)?,
        };

        let (source_agent_id, source_chat_id) = if target_agent.is_some() {
            (Some(self.agent_id.clone()), Some(self.chat_id.clone()))
        } else {
            (None, None)
        };

        let task = self
            .task_service
            .create_cron_template(
                &self.user_id,
                &agent_id,
                title,
                instruction,
                cron_expression,
                next_run_at,
                source_agent_id,
                source_chat_id,
                run_at,
            )
            .await?;

        Ok(ToolOutput::text(serde_json::json!({
            "task_id": task.id,
            "cron_expression": cron_expression,
            "next_run_at": next_run_at.to_rfc3339(),
            "message": format!("Cron job '{}' created. Next run at {}.", title, next_run_at.format("%Y-%m-%d %H:%M UTC"))
        }).to_string()))
    }

    async fn handle_delete(&self, arguments: &Value) -> Result<ToolOutput, AppError> {
        let task_id = arguments
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'task_id' parameter".into()))?;

        let task = self
            .task_service
            .find_by_id(task_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Cron job not found".into()))?;

        if task.user_id != self.user_id {
            return Err(AppError::Forbidden("Not your task".into()));
        }

        self.task_service.mark_cancelled(task_id).await?;

        Ok(ToolOutput::text(serde_json::json!({
            "message": format!("Cron job '{}' cancelled.", task.title)
        }).to_string()))
    }

    async fn handle_list(&self, arguments: &Value) -> Result<ToolOutput, AppError> {
        let target_agent = arguments.get("target_agent").and_then(|v| v.as_str());
        let _ = self.resolve_agent_id(target_agent).await?;

        let all_tasks = self.task_service.list_active(&self.user_id).await.unwrap_or_default();

        let cron_tasks: Vec<_> = all_tasks
            .into_iter()
            .filter(|t| matches!(t.kind, crate::agent::task::models::TaskKind::Cron { .. }))
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "description": t.description,
                    "agent_id": t.agent_id,
                    "kind": t.kind,
                    "status": t.status,
                })
            })
            .collect();

        Ok(ToolOutput::text(serde_json::json!({
            "scheduled_tasks": cron_tasks,
            "count": cron_tasks.len(),
        }).to_string()))
    }
}

#[async_trait]
impl AgentTool for ScheduleTaskTool {
    fn name(&self) -> &str {
        "schedule"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "schedule_task".to_string(),
            description: "Create, delete, or list cron jobs. A cron is deterministic: it runs a \
                fixed instruction at exact, recurring times based on a cron expression. Each run \
                executes the instruction verbatim — the agent follows the instruction, not its own \
                judgment. All runs share a single persistent chat with full history. For one-off \
                work (immediate or deferred to a specific time), use delegate_task or run_subtask with run_at. \
                For periodic autonomous check-ins, use set_heartbeat + HEARTBEAT.md.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "delete", "list"],
                        "description": "The action to perform"
                    },
                    "target_agent": {
                        "type": "string",
                        "description": "Optional: agent name to schedule for (from <available_agents>). Omit to schedule for yourself."
                    },
                    "cron_expression": {
                        "type": "string",
                        "description": "5-field cron expression (minute hour day-of-month month day-of-week). Required for 'create'."
                    },
                    "title": {
                        "type": "string",
                        "description": "Short title for the cron job. Optional for 'create'."
                    },
                    "instruction": {
                        "type": "string",
                        "description": "The exact instruction to execute each time the cron fires. Runs verbatim every occurrence. Required for 'create'."
                    },
                    "run_at": {
                        "type": "string",
                        "description": "Optional ISO 8601 datetime for the first run (e.g. '2026-03-15T09:00:00Z'). Defers the first cron firing to this time. If omitted, the first run is the next natural cron occurrence."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "The cron job ID to cancel. Required for 'delete'."
                    }
                },
                "required": ["action"]
            }),
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'action' parameter".into()))?;

        match action {
            "create" => self.handle_create(&arguments).await,
            "delete" => self.handle_delete(&arguments).await,
            "list" => self.handle_list(&arguments).await,
            _ => Err(AppError::Validation(format!("Unknown action: {}", action))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn parse_cron_valid_every_minute() {
        let schedule = parse_cron("* * * * *").unwrap();
        let next = schedule.upcoming(Utc).next().unwrap();
        assert!(next > Utc::now());
    }

    #[test]
    fn parse_cron_valid_daily_9am() {
        let schedule = parse_cron("0 9 * * *").unwrap();
        let next = schedule.upcoming(Utc).next().unwrap();
        assert_eq!(next.minute(), 0);
        assert_eq!(next.hour(), 9);
    }

    #[test]
    fn parse_cron_valid_weekdays_at_noon() {
        let schedule = parse_cron("0 12 * * MON-FRI").unwrap();
        let next = schedule.upcoming(Utc).next().unwrap();
        assert_eq!(next.hour(), 12);
        assert_eq!(next.minute(), 0);
        let weekday = next.weekday().num_days_from_monday();
        assert!(weekday < 5, "Should be a weekday (Mon=0 .. Fri=4), got {weekday}");
    }

    #[test]
    fn parse_cron_valid_every_30_mins() {
        let schedule = parse_cron("*/30 * * * *").unwrap();
        let occurrences: Vec<_> = schedule.upcoming(Utc).take(4).collect();
        assert_eq!(occurrences.len(), 4);
        for occ in &occurrences {
            assert!(occ.minute() == 0 || occ.minute() == 30);
        }
    }

    #[test]
    fn parse_cron_invalid_expression() {
        let result = parse_cron("not a cron");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid cron expression"), "Error: {err}");
    }

    #[test]
    fn parse_cron_rejects_empty() {
        assert!(parse_cron("").is_err());
    }

    #[test]
    fn parse_cron_rejects_6_fields() {
        let result = parse_cron("0 0 9 * * MON");
        assert!(result.is_err());
    }

    #[test]
    fn parse_cron_rejects_3_fields() {
        let result = parse_cron("0 9 *");
        assert!(result.is_err());
    }

    #[test]
    fn next_cron_occurrence_returns_future() {
        let next = next_cron_occurrence("* * * * *").unwrap();
        assert!(next > Utc::now());
    }

    #[test]
    fn next_cron_occurrence_daily_has_correct_time() {
        let next = next_cron_occurrence("30 14 * * *").unwrap();
        assert_eq!(next.hour(), 14);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn next_cron_occurrence_multiple_calls_are_consistent() {
        let a = next_cron_occurrence("0 0 * * *").unwrap();
        let b = next_cron_occurrence("0 0 * * *").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn next_cron_occurrence_invalid_returns_error() {
        assert!(next_cron_occurrence("invalid").is_err());
    }
}
