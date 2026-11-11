use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::agent::service::AgentService;
use crate::agent::task::executor::TaskExecutor;
use crate::agent::task::models::CreateTaskRequest;
use crate::agent::task::service::TaskService;
use crate::core::error::AppError;
use crate::policy::models::PolicyAction;
use crate::policy::service::PolicyService;
use frona_derive::agent_tool;
use frona_text::prompt::PromptLoader;

use super::{InferenceContext, ToolOutput, active_chat};

/// XOR: both together is rejected because the schema's own root
/// `description` already covers the prose case.
fn parse_result_spec(
    arguments: &Value,
    tool_name: &str,
    process_result: bool,
) -> Result<(Option<Value>, Option<String>), AppError> {
    let result_schema = arguments.get("result_schema").cloned();
    let result_description = arguments
        .get("result_description")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    match (result_schema, result_description) {
        (None, None) => Err(AppError::Validation(format!(
            "{tool_name} requires either 'result_description' (one-line prose describing the result the executing agent should produce — default for human-facing tasks) or 'result_schema' (JSON Schema for structured agent-to-agent handoff). See the tool docs for examples."
        ))),
        (Some(_), Some(_)) => Err(AppError::Validation(
            "Pass either 'result_description' OR 'result_schema', not both. Use 'result_description' for prose results; use 'result_schema' when the executing agent must produce a typed shape.".into(),
        )),
        (Some(schema), None) => {
            crate::agent::task::schema::validate_schema_doc(&schema)
                .map_err(AppError::Validation)?;
            if !crate::agent::task::schema::is_simple_schema(&schema) {
                if !process_result {
                    return Err(AppError::Validation(
                        "Complex result_schema (nested objects, arrays of objects, etc.) cannot be rendered deterministically. Set process_result=true so the parent agent can render the structured result, or simplify the schema to a top-level scalar / array-of-scalars / oneOf-of-scalars / object-with-scalar-properties.".into(),
                    ));
                }
                if !crate::agent::task::schema::has_renderable_summary_field(&schema) {
                    return Err(AppError::Validation(
                        "Complex result_schema must include a required top-level `summary` string property — the user-facing renderer only shows that field when the schema is complex. Either add `summary: { type: \"string\" }` to `required`, or simplify the schema to top-level scalar / array-of-scalars / object-with-scalar-properties.".into(),
                    ));
                }
            }
            Ok((Some(schema), None))
        }
        (None, Some(desc)) => Ok((None, Some(desc))),
    }
}

pub fn parse_cron(expression: &str) -> Result<cron::Schedule, AppError> {
    let seven_field = format!("0 {} *", expression);
    cron::Schedule::from_str(&seven_field).map_err(|e| {
        AppError::Validation(format!("Invalid cron expression '{}': {}", expression, e))
    })
}

pub fn next_cron_occurrence(expression: &str, timezone: &str) -> Result<DateTime<Utc>, AppError> {
    let schedule = parse_cron(expression)?;
    let tz: chrono_tz::Tz = timezone.parse().map_err(|e| {
        AppError::Validation(format!(
            "Invalid timezone '{}': {}. Use an IANA name like 'America/Los_Angeles', 'Asia/Tokyo', or 'UTC'.",
            timezone, e
        ))
    })?;
    schedule
        .upcoming(tz)
        .next()
        .map(|dt| dt.with_timezone(&Utc))
        .ok_or_else(|| AppError::Validation("Cron expression has no future occurrences".into()))
}

pub struct TaskTool {
    task_service: TaskService,
    agent_service: AgentService,
    task_executor: Arc<TaskExecutor>,
    policy_service: PolicyService,
    prompts: PromptLoader,
    server_timezone: String,
}

impl TaskTool {
    pub fn new(
        task_service: TaskService,
        agent_service: AgentService,
        task_executor: Arc<TaskExecutor>,
        policy_service: PolicyService,
        prompts: PromptLoader,
        server_timezone: String,
    ) -> Self {
        Self {
            task_service,
            agent_service,
            task_executor,
            policy_service,
            prompts,
            server_timezone,
        }
    }

    fn resolve_timezone(
        &self,
        arguments: &Value,
        user: &crate::auth::models::User,
    ) -> Result<String, AppError> {
        if let Some(arg) = arguments.get("timezone").and_then(|v| v.as_str()) {
            arg.parse::<chrono_tz::Tz>().map_err(|e| {
                AppError::Validation(format!(
                    "Invalid timezone '{}': {}. Use an IANA name like 'America/Los_Angeles', 'Asia/Tokyo', or 'UTC'.",
                    arg, e
                ))
            })?;
            return Ok(arg.to_string());
        }
        Ok(user.resolved_timezone(&self.server_timezone))
    }

    async fn resolve_target_agent(
        &self,
        ctx: &InferenceContext,
        target_agent_name: Option<&str>,
    ) -> Result<(crate::agent::models::Agent, bool), AppError> {
        let user_id = &ctx.user.id;
        let agent_id = &ctx.agent.id;
        match target_agent_name {
            Some(name) => {
                let by_handle = self.agent_service.find_by_handle(user_id, name).await?;
                let agent = match by_handle {
                    Some(a) => a,
                    None => self
                        .agent_service
                        .find_by_name(user_id, name)
                        .await?
                        .ok_or_else(|| {
                            AppError::Validation(format!(
                                "Agent '{}' not found. Check <available_agents> for valid agent names.",
                                name
                            ))
                        })?,
                };
                if !agent.enabled {
                    return Err(AppError::Validation(format!(
                        "Agent '{}' is disabled and cannot accept tasks.",
                        agent.name
                    )));
                }
                let is_self = agent.id == *agent_id;
                Ok((agent, is_self))
            }
            None => Ok((ctx.agent.clone(), true)),
        }
    }

    async fn authorize_delegation(
        &self,
        ctx: &InferenceContext,
        target_agent: &crate::agent::models::Agent,
        is_self: bool,
    ) -> Result<Option<ToolOutput>, AppError> {
        if is_self {
            return Ok(None);
        }
        let decision = self
            .policy_service
            .authorize(
                &ctx.user.id,
                &ctx.agent,
                PolicyAction::DelegateTask {
                    target_agent_id: target_agent.id.clone(),
                    target_handle: target_agent.handle.clone(),
                },
            )
            .await?;
        if decision.is_denied() {
            return Ok(Some(ToolOutput::error(format!(
                "Authorization denied: agent '{}' is not permitted to delegate tasks to '{}'.",
                ctx.agent.name, target_agent.name
            ))));
        }
        Ok(None)
    }

    async fn handle_create_task(
        &self,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let chat = active_chat(ctx)?;
        let user_id = &ctx.user.id;
        let agent_id = &ctx.agent.id;
        let chat_id = &chat.id;
        let space_id = chat.space_id.clone();

        let title = arguments
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'title' parameter".into()))?;
        let instruction = arguments
            .get("instruction")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'instruction' parameter".into()))?;
        let target_agent_name = arguments.get("target_agent").and_then(|v| v.as_str());
        let process_result = arguments
            .get("process_result")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if arguments.get("cron_expression").is_some() {
            return Err(AppError::Validation(
                "cron_expression is no longer accepted by create_task. Use create_recurring_task for scheduled work.".into(),
            ));
        }

        let has_delay_minutes = arguments
            .get("delay_minutes")
            .and_then(|v| v.as_u64())
            .is_some();
        let has_run_at = arguments.get("run_at").is_some();
        if has_delay_minutes && has_run_at {
            return Err(AppError::Validation(
                "Cannot use both delay_minutes and run_at.".into(),
            ));
        }

        let (target_agent, is_self) = self.resolve_target_agent(ctx, target_agent_name).await?;
        if let Some(denied) = self
            .authorize_delegation(ctx, &target_agent, is_self)
            .await?
        {
            return Ok(denied);
        }

        if chat.task_id.is_some() {
            if has_delay_minutes || has_run_at {
                return Err(AppError::Validation(
                    "Cannot create a deferred task from inside a running task. Use `defer_task` to retry the current task later instead of scheduling a duplicate.".into(),
                ));
            }
            if is_self {
                return Err(AppError::Validation(
                    "Cannot create a self-targeted task from inside a running task. Do the work directly, or delegate to a different agent via `target_agent`.".into(),
                ));
            }
        }

        let timezone = self.resolve_timezone(&arguments, &ctx.user)?;

        self.handle_create_oneoff(
            user_id,
            agent_id,
            chat_id,
            space_id,
            &target_agent,
            is_self,
            process_result,
            title,
            instruction,
            &timezone,
            &arguments,
        )
        .await
    }

    async fn handle_create_recurring(
        &self,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let chat = active_chat(ctx)?;
        let user_id = &ctx.user.id;
        let agent_id = &ctx.agent.id;
        let chat_id = &chat.id;
        let space_id = chat.space_id.clone();

        let title = arguments
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'title' parameter".into()))?;
        let instruction = arguments
            .get("instruction")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'instruction' parameter".into()))?;
        let cron_expression = arguments
            .get("cron_expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'cron_expression' parameter".into()))?;
        let target_agent_name = arguments.get("target_agent").and_then(|v| v.as_str());

        if arguments.get("delay_minutes").is_some() || arguments.get("run_at").is_some() {
            return Err(AppError::Validation(
                "delay_minutes and run_at are not allowed on create_recurring_task. The cron_expression controls when the task fires.".into(),
            ));
        }

        let (target_agent, is_self) = self.resolve_target_agent(ctx, target_agent_name).await?;
        if let Some(denied) = self
            .authorize_delegation(ctx, &target_agent, is_self)
            .await?
        {
            return Ok(denied);
        }

        let timezone = self.resolve_timezone(&arguments, &ctx.user)?;
        self.handle_create_recurring_internal(
            user_id,
            agent_id,
            chat_id,
            space_id,
            &target_agent,
            is_self,
            title,
            instruction,
            cron_expression,
            &timezone,
            &arguments,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_create_recurring_internal(
        &self,
        user_id: &str,
        agent_id: &str,
        chat_id: &str,
        space_id: Option<String>,
        target_agent: &crate::agent::models::Agent,
        is_self: bool,
        title: &str,
        instruction: &str,
        cron_expression: &str,
        timezone: &str,
        arguments: &Value,
    ) -> Result<ToolOutput, AppError> {
        let run_at = super::resolve_run_at(arguments, timezone)?;

        parse_cron(cron_expression)?;
        let next_run_at = match run_at {
            Some(dt) => dt,
            None => next_cron_occurrence(cron_expression, timezone)?,
        };

        let source_agent_id = if is_self {
            None
        } else {
            Some(agent_id.to_string())
        };

        use crate::agent::task::models::{CronConcurrency, CronMode};
        let cron_mode = match arguments.get("cron_mode").and_then(|v| v.as_str()) {
            None => CronMode::Singleton,
            Some("singleton") => CronMode::Singleton,
            Some("per_instance") => CronMode::PerInstance,
            Some(other) => {
                return Err(AppError::Validation(format!(
                    "Invalid cron_mode '{}'. Use 'singleton' or 'per_instance'.",
                    other
                )));
            }
        };
        let cron_concurrency = match arguments.get("cron_concurrency").and_then(|v| v.as_str()) {
            Some("allow") => CronConcurrency::Allow,
            Some("forbid") => CronConcurrency::Forbid,
            Some("replace") => CronConcurrency::Replace,
            None => match cron_mode {
                CronMode::Singleton => CronConcurrency::Replace,
                CronMode::PerInstance => CronConcurrency::Forbid,
            },
            Some(other) => {
                return Err(AppError::Validation(format!(
                    "Invalid cron_concurrency '{}'. Use 'allow', 'forbid', or 'replace'.",
                    other
                )));
            }
        };
        let process_result = arguments
            .get("process_result")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let (result_schema, result_description) =
            parse_result_spec(arguments, "create_recurring_task", process_result)?;

        let task = self
            .task_service
            .create_cron_template(
                user_id,
                &target_agent.id,
                title,
                instruction,
                cron_expression,
                timezone.to_string(),
                next_run_at,
                space_id,
                source_agent_id,
                Some(chat_id.to_string()),
                run_at,
                cron_mode,
                cron_concurrency,
                process_result,
                result_schema,
                result_description,
            )
            .await?;

        let tz: chrono_tz::Tz = timezone.parse().expect("timezone was validated earlier");
        let next_local = next_run_at.with_timezone(&tz);

        Ok(ToolOutput::text(
            serde_json::json!({
                "task_id": task.id,
                "cron_expression": cron_expression,
                "timezone": timezone,
                "next_run_at": next_run_at.to_rfc3339(),
                "message": format!(
                    "Cron job '{}' created for {}. Next run at {} ({}).",
                    title, target_agent.name, next_local.format("%Y-%m-%d %H:%M %Z"), timezone
                )
            })
            .to_string(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_create_oneoff(
        &self,
        user_id: &str,
        agent_id: &str,
        chat_id: &str,
        space_id: Option<String>,
        target_agent: &crate::agent::models::Agent,
        is_self: bool,
        process_result: bool,
        title: &str,
        instruction: &str,
        timezone: &str,
        arguments: &Value,
    ) -> Result<ToolOutput, AppError> {
        let run_at = super::resolve_run_at(arguments, timezone)?;

        let (result_schema, result_description) =
            parse_result_spec(arguments, "create_task", process_result)?;

        // source_agent_id set even for self-targets so kind=Delegation;
        // Direct has no resume_parent machinery.
        let source_agent_id = Some(agent_id.to_string());

        let req = CreateTaskRequest {
            agent_id: target_agent.id.clone(),
            space_id,
            chat_id: None,
            title: title.to_string(),
            description: Some(instruction.to_string()),
            source_agent_id,
            source_chat_id: Some(chat_id.to_string()),
            resume_parent: Some(process_result),
            run_at,
            quarantined: false,
            result_schema,
            result_description,
        };

        let task_response = self.task_service.create(user_id, req).await?;
        let task_id = task_response.id.clone();

        if run_at.is_none() {
            let exec = self.task_executor.clone();
            let tid = task_id.clone();
            tokio::spawn(async move {
                if let Err(e) = exec.run_task_by_id(&tid).await {
                    tracing::warn!(error = %e, task_id = %tid, "Failed to run task execution immediately");
                }
            });
        }

        let tz: chrono_tz::Tz = timezone.parse().expect("timezone was validated earlier");
        let format_local = |at: DateTime<Utc>| {
            at.with_timezone(&tz)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
        };

        let message = if is_self {
            match run_at {
                Some(at) => format!(
                    "Task '{}' created, deferred until {}.",
                    title,
                    format_local(at)
                ),
                None => format!("Task '{}' created and running.", title),
            }
        } else {
            match (run_at, process_result) {
                (Some(at), _) => format!(
                    "Task '{}' assigned to {}, deferred until {}.",
                    title,
                    target_agent.name,
                    format_local(at)
                ),
                (None, false) => format!(
                    "Task '{}' assigned to {}. Results will be posted to this chat when complete.",
                    title, target_agent.name
                ),
                (None, true) => format!(
                    "Task '{}' assigned to {}. You will be resumed with the result.",
                    title, target_agent.name
                ),
            }
        };

        Ok(ToolOutput::text(
            serde_json::json!({
                "task_id": task_id,
                "target_agent": target_agent.name,
                "run_at": run_at.map(|t| t.to_rfc3339()),
                "message": message,
            })
            .to_string(),
        ))
    }

    async fn handle_list(&self, ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
        let all_tasks = self
            .task_service
            .list_active(&ctx.user.id)
            .await
            .unwrap_or_default();

        let tasks: Vec<_> = all_tasks
            .into_iter()
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

        Ok(ToolOutput::text(
            serde_json::json!({
                "tasks": tasks,
                "count": tasks.len(),
            })
            .to_string(),
        ))
    }

    async fn handle_delete(
        &self,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let task_id = arguments
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'task_id' parameter".into()))?;

        let task = self.task_service.cancel(&ctx.user.id, task_id).await?;
        self.task_executor.cancel_task(task_id).await;

        Ok(ToolOutput::text(
            serde_json::json!({
                "message": format!("Task '{}' cancelled.", task.title)
            })
            .to_string(),
        ))
    }
}

#[agent_tool(
    name = "task",
    files("create_task", "create_recurring_task", "list_tasks", "delete_task")
)]
impl TaskTool {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        match tool_name {
            "create_task" => self.handle_create_task(arguments, ctx).await,
            "create_recurring_task" => self.handle_create_recurring(arguments, ctx).await,
            "list_tasks" => self.handle_list(ctx).await,
            "delete_task" => self.handle_delete(arguments, ctx).await,
            _ => Err(AppError::Validation(format!(
                "Unknown task tool: {}",
                tool_name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn parse_result_spec_requires_one_of_the_two() {
        let err = parse_result_spec(&serde_json::json!({}), "create_task", false).unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(msg.contains("result_description"));
                assert!(msg.contains("result_schema"));
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_spec_rejects_both_together() {
        let args = serde_json::json!({
            "result_description": "a string",
            "result_schema": { "type": "string" }
        });
        let err = parse_result_spec(&args, "create_task", false).unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("not both")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_spec_description_only_returns_description() {
        let args = serde_json::json!({ "result_description": "A research report" });
        let (schema, desc) = parse_result_spec(&args, "create_task", false).unwrap();
        assert!(schema.is_none());
        assert_eq!(desc, Some("A research report".into()));
    }

    #[test]
    fn parse_result_spec_blank_description_is_rejected_as_missing() {
        let args = serde_json::json!({ "result_description": "   " });
        let err = parse_result_spec(&args, "create_task", false).unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("result_description")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_spec_schema_only_validates_simple() {
        let args = serde_json::json!({ "result_schema": { "type": "string" } });
        let (schema, desc) = parse_result_spec(&args, "create_task", false).unwrap();
        assert!(schema.is_some());
        assert!(desc.is_none());
    }

    #[test]
    fn parse_result_spec_complex_schema_requires_process_result() {
        let args = serde_json::json!({
            "result_schema": {
                "type": "object",
                "properties": {
                    "phones": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "name": { "type": "string" } } }
                    }
                }
            }
        });
        let err = parse_result_spec(&args, "create_task", false).unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("Complex result_schema")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

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
        assert!(
            weekday < 5,
            "Should be a weekday (Mon=0 .. Fri=4), got {weekday}"
        );
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
        let next = next_cron_occurrence("* * * * *", "UTC").unwrap();
        assert!(next > Utc::now());
    }

    #[test]
    fn next_cron_occurrence_daily_has_correct_time() {
        let next = next_cron_occurrence("30 14 * * *", "UTC").unwrap();
        assert_eq!(next.hour(), 14);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn next_cron_occurrence_multiple_calls_are_consistent() {
        let a = next_cron_occurrence("0 0 * * *", "UTC").unwrap();
        let b = next_cron_occurrence("0 0 * * *", "UTC").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn next_cron_occurrence_invalid_returns_error() {
        assert!(next_cron_occurrence("invalid", "UTC").is_err());
    }

    #[test]
    fn next_cron_occurrence_resolves_in_named_tz() {
        // 8am LA → 16:00 UTC (winter, PST) or 15:00 UTC (summer, PDT). Verify
        // the time when projected back into the LA clock is 08:00.
        let next = next_cron_occurrence("0 8 * * *", "America/Los_Angeles").unwrap();
        let la: chrono_tz::Tz = "America/Los_Angeles".parse().unwrap();
        let next_la = next.with_timezone(&la);
        assert_eq!(next_la.hour(), 8);
        assert_eq!(next_la.minute(), 0);
    }

    #[test]
    fn next_cron_occurrence_in_tokyo() {
        let next = next_cron_occurrence("0 8 * * *", "Asia/Tokyo").unwrap();
        let tokyo: chrono_tz::Tz = "Asia/Tokyo".parse().unwrap();
        let next_tokyo = next.with_timezone(&tokyo);
        assert_eq!(next_tokyo.hour(), 8);
        assert_eq!(next_tokyo.minute(), 0);
    }

    #[test]
    fn next_cron_occurrence_invalid_tz_rejected() {
        let err = next_cron_occurrence("0 8 * * *", "Mars/Olympus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Invalid timezone"), "got: {msg}");
        assert!(msg.contains("IANA"), "expected IANA hint, got: {msg}");
    }
}
