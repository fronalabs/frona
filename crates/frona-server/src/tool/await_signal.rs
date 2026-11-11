use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::agent::signal::{SignalService, Watch};
use crate::agent::task::models::SignalMode;
use crate::agent::task::schema::validate_schema_doc;
use crate::agent::task::service::TaskService;
use crate::chat::broadcast::BroadcastService;
use crate::core::error::AppError;
use crate::tool::{
    AgentTool, InferenceContext, ToolDefinition, ToolOutput, active_chat, load_tool_definition,
};
use frona_text::prompt::PromptLoader;

pub struct AwaitSignalTool {
    pub task_service: TaskService,
    pub signal_service: Arc<SignalService>,
    pub broadcast_service: BroadcastService,
    pub prompts: PromptLoader,
    pub default_max_evaluations: u32,
    pub default_max_continuous_evaluations: u32,
    pub server_timezone: String,
}

impl AwaitSignalTool {
    pub fn new(
        task_service: TaskService,
        signal_service: Arc<SignalService>,
        broadcast_service: BroadcastService,
        prompts: PromptLoader,
        default_max_evaluations: u32,
        default_max_continuous_evaluations: u32,
        server_timezone: String,
    ) -> Self {
        Self {
            task_service,
            signal_service,
            broadcast_service,
            prompts,
            default_max_evaluations,
            default_max_continuous_evaluations,
            server_timezone,
        }
    }
}

#[async_trait]
impl AgentTool for AwaitSignalTool {
    fn name(&self) -> &str {
        "await_signal"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        load_tool_definition(&self.prompts, "tools/await_signal.md")
            .map(|d| vec![d])
            .unwrap_or_default()
    }

    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let chat = active_chat(ctx)?;
        let title = arguments
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: title".into()))?
            .trim()
            .to_string();
        if title.is_empty() {
            return Err(AppError::Validation("title must not be empty".into()));
        }
        if title.len() > 256 {
            return Err(AppError::Validation(
                "title must be at most 256 bytes".into(),
            ));
        }
        let description = arguments
            .get("instructions")
            .or_else(|| arguments.get("description"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing required parameter: instructions".into()))?
            .to_string();
        // Accept both `expected_categories` (current) and `tags` (legacy alias)
        // for the categorical-match list.
        let expected_categories = if arguments.get("expected_categories").is_some() {
            parse_string_array(&arguments, "expected_categories")?
        } else {
            parse_string_array(&arguments, "tags")?
        };
        let expected_channels = parse_string_array(&arguments, "expected_channels")?;
        let expected_contacts = parse_string_array(&arguments, "expected_contacts")?;
        let tz = ctx.user.resolved_timezone(&self.server_timezone);
        let expires_at = resolve_expires_at(&arguments, &tz)?;
        let resume_parent = arguments
            .get("resume_parent")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mode = parse_mode(&arguments)?;
        let default_max = match mode {
            SignalMode::Once => self.default_max_evaluations,
            SignalMode::Continuous => self.default_max_continuous_evaluations,
        };
        let max_evaluations = arguments
            .get("max_evaluations")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(MAX_EVALUATIONS_CAP) as u32)
            .unwrap_or(default_max);

        if expected_categories.is_empty()
            && expected_channels.is_empty()
            && expected_contacts.is_empty()
        {
            return Err(AppError::Validation(
                "await_signal requires at least one of: expected_categories, expected_channels, expected_contacts"
                    .into(),
            ));
        }

        let result_schema = arguments.get("result_schema").cloned();
        if let Some(ref schema) = result_schema {
            validate_schema_doc(schema).map_err(AppError::Validation)?;
        }

        let task = self
            .task_service
            .create_signal(
                &ctx.user.id,
                ctx.agent.id.clone(),
                chat.id.clone(),
                title,
                description,
                resume_parent,
                mode,
                expected_categories,
                expected_channels,
                expected_contacts,
                expires_at,
                max_evaluations,
                result_schema,
            )
            .await?;

        if let Some(watch) = Watch::from_task(&task) {
            self.signal_service.register(watch).await;
        }

        self.broadcast_service.broadcast_task_update(
            &ctx.user.id,
            &task.id,
            "pending",
            &task.title,
            None,
            Some(&chat.id),
            None,
        );

        let body = match mode {
            SignalMode::Once => format!(
                "Signal task created (id: {}). The current chat will resume when a matching candidate arrives and you confirm it via complete_task in the signal task's chat.",
                task.id
            ),
            SignalMode::Continuous => format!(
                "Continuous signal task created (id: {}). Each matching candidate will invoke the signal task agent; use report_signal to record matches without ending the watch. complete_task stops monitoring.",
                task.id
            ),
        };
        Ok(ToolOutput::text(body))
    }
}

const MAX_EVALUATIONS_CAP: u64 = 1_000;

fn parse_mode(arguments: &Value) -> Result<SignalMode, AppError> {
    let Some(value) = arguments.get("mode") else {
        return Ok(SignalMode::Once);
    };
    let s = value
        .as_str()
        .ok_or_else(|| AppError::Validation("mode must be a string".into()))?;
    match s {
        "once" => Ok(SignalMode::Once),
        "continuous" => Ok(SignalMode::Continuous),
        other => Err(AppError::Validation(format!(
            "mode must be \"once\" or \"continuous\" (got {other:?})"
        ))),
    }
}

fn resolve_expires_at(arguments: &Value, tz: &str) -> Result<Option<DateTime<Utc>>, AppError> {
    let has_minutes = arguments.get("expires_in_minutes").is_some();
    let has_absolute = arguments.get("expires_at").is_some();

    if has_minutes && has_absolute {
        return Err(AppError::Validation(
            "Cannot use both expires_in_minutes and expires_at.".into(),
        ));
    }

    if has_minutes {
        let mins = arguments
            .get("expires_in_minutes")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                AppError::Validation("expires_in_minutes must be a non-negative integer".into())
            })?;
        if mins == 0 {
            return Err(AppError::Validation(
                "expires_in_minutes must be greater than 0".into(),
            ));
        }
        return Ok(Some(Utc::now() + Duration::minutes(mins as i64)));
    }

    let Some(value) = arguments.get("expires_at") else {
        return Ok(None);
    };
    super::parse_run_at(value, tz)
        .map_err(|e| AppError::Validation(e.to_string().replace("run_at", "expires_at")))
}

fn parse_string_array(args: &Value, key: &str) -> Result<Vec<String>, AppError> {
    let Some(arr) = args.get(key) else {
        return Ok(Vec::new());
    };
    let arr = arr
        .as_array()
        .ok_or_else(|| AppError::Validation(format!("{key} must be an array of strings")))?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let s = v
            .as_str()
            .ok_or_else(|| AppError::Validation(format!("{key} entries must be strings")))?;
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(AppError::Validation(format!(
                "{key} entries must be non-empty"
            )));
        }
        out.push(trimmed.to_string());
    }
    Ok(out)
}
