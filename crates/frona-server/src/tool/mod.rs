pub mod annotate;
pub mod await_signal;
pub mod browser;
pub mod cli;
pub mod create_agent;
pub mod files;
pub mod heartbeat;
mod image;
pub mod manage_app;
pub mod manage_policy;
pub mod manager;
pub mod mcp;
pub mod notify_human;
pub mod produce_file;
pub mod provider;
pub mod registry;
pub mod report_signal;
pub mod request_credentials;
pub mod sandbox;
pub mod send_message;
pub mod task;
pub mod task_control;
pub mod update_identity;
pub mod voice;
pub mod web_fetch;
pub mod web_search;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::error::AppError;

pub use crate::inference::request::{InferenceContext, active_chat};

use crate::agent::prompt::PromptLoader;

/// Read a string argument out of a tool call: present, a string, and not blank once
/// trimmed. `None` for anything else - a missing key, a non-string, or whitespace.
///
/// The **pull** only, deliberately not the policy. What a tool does about an absent
/// argument is a real choice, and its callers make opposite ones for good reasons: the
/// Classify's tools answer with `ToolOutput::text` naming the argument and saying what
/// belongs in it, because the model is mid-conversation and can just try again, while
/// `memory_search` raises `AppError::Validation`. Sharing the four combinators leaves that
/// decision one visible line at each site instead of a chain each site re-derives -
/// including the trimming and blank-check, which is where they had quietly diverged.
pub fn str_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Accepts unix timestamp or naive ISO 8601 (interpreted in `tz`). Rejects
/// offset-bearing strings - the agent must use naive + `timezone` parameter.
pub fn parse_run_at(
    value: &Value,
    tz: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, AppError> {
    let dt = match value {
        Value::Number(n) => {
            let ts = n
                .as_i64()
                .ok_or_else(|| AppError::Validation("Invalid run_at timestamp".into()))?;
            Some(
                chrono::DateTime::from_timestamp(ts, 0)
                    .ok_or_else(|| AppError::Validation("Invalid run_at timestamp".into()))?,
            )
        }
        Value::String(s) => {
            if let Ok(ts) = s.parse::<i64>() {
                Some(
                    chrono::DateTime::from_timestamp(ts, 0)
                        .ok_or_else(|| AppError::Validation("Invalid run_at timestamp".into()))?,
                )
            } else {
                Some(parse_naive_run_at(s, tz)?)
            }
        }
        _ => None,
    };

    if let Some(at) = dt
        && at <= chrono::Utc::now()
    {
        return Err(AppError::Validation("run_at must be in the future".into()));
    }

    Ok(dt)
}

fn parse_naive_run_at(s: &str, tz: &str) -> Result<chrono::DateTime<chrono::Utc>, AppError> {
    // RFC 3339 parse succeeds = offset-bearing. Reject - bypasses per-task TZ.
    if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
        return Err(AppError::Validation(format!(
            "run_at '{}' includes an explicit UTC offset. Use a naive ISO 8601 form like '2026-05-20T22:00:00' (interpreted in the user's local timezone) and set the optional `timezone` parameter only if the user names a different zone.",
            s
        )));
    }

    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .map_err(|e| {
            AppError::Validation(format!(
                "Invalid run_at datetime '{}': {}. Use naive ISO 8601 form like '2026-05-20T22:00:00' (no 'Z', no offset).",
                s, e
            ))
        })?;

    let parsed_tz: chrono_tz::Tz = tz.parse().map_err(|e| {
        AppError::Validation(format!(
            "Invalid timezone '{}': {}. Use an IANA name like 'America/Los_Angeles', 'Asia/Tokyo', or 'UTC'.",
            tz, e
        ))
    })?;

    use chrono::TimeZone;
    let resolved = parsed_tz
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| {
            AppError::Validation(format!(
                "run_at '{}' is ambiguous or invalid in timezone '{}' (likely a DST transition). Pick a different time.",
                s, tz
            ))
        })?;

    Ok(resolved.with_timezone(&chrono::Utc))
}

/// Resolve a `run_at` datetime from arguments, supporting both `run_at` and `delay_minutes`.
/// `delay_minutes` takes precedence over `run_at` if both are provided.
pub fn resolve_run_at(
    arguments: &Value,
    tz: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, AppError> {
    if let Some(delay) = arguments.get("delay_minutes").and_then(|v| v.as_u64()) {
        if delay == 0 {
            return Err(AppError::Validation(
                "delay_minutes must be greater than 0".into(),
            ));
        }
        return Ok(Some(
            chrono::Utc::now() + chrono::Duration::minutes(delay as i64),
        ));
    }

    match arguments.get("run_at") {
        Some(v) => parse_run_at(v, tz),
        None => Ok(None),
    }
}

pub fn is_tool_available(state: &crate::core::state::AppState, tool_name: &str) -> bool {
    match tool_name {
        "voice_call" => state.voice_provider.is_some(),
        _ => true,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub id: String,
    pub provider_id: String,
    pub description: String,
    pub parameters: Value,
}

pub struct ImageData {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

pub struct ToolOutput {
    text: String,
    images: Vec<ImageData>,
    attachments: Vec<crate::storage::Attachment>,
    /// Pause marker - when `Some(_)` with `status == Pending`, the tool loop
    /// exits with `ExternalToolPending`. Mutually exclusive with `task_event`.
    hitl: Option<crate::inference::hitl::Hitl>,
    /// Terminal signal - when `Some(_)`, the tool loop exits as Completed
    /// with this as the lifecycle event. Mutually exclusive with `hitl`.
    task_event: Option<crate::inference::tool_call::TaskEvent>,
    system_prompt: Option<String>,
    /// Non-HITL pause flag for tools whose work resolves via an external
    /// system callback (Twilio webhook for voice tools, etc.) rather than
    /// user input. The tool loop exits with `ExternalToolPending` and the
    /// external system later calls `resolve_tool_call` directly. Distinct
    /// from `hitl` which carries the typed user-prompt payload.
    pending_external: bool,
    success: bool,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            images: Vec::new(),
            attachments: Vec::new(),
            hitl: None,
            task_event: None,
            system_prompt: None,
            pending_external: false,
            success: true,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            images: Vec::new(),
            attachments: Vec::new(),
            hitl: None,
            task_event: None,
            system_prompt: None,
            pending_external: false,
            success: false,
        }
    }

    pub fn mixed(text: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self {
            text: text.into(),
            images,
            attachments: Vec::new(),
            hitl: None,
            task_event: None,
            system_prompt: None,
            pending_external: false,
            success: true,
        }
    }

    pub fn with_attachment(mut self, a: crate::storage::Attachment) -> Self {
        self.attachments.push(a);
        self
    }

    /// Attach a HITL pause marker. The tool loop detects this and exits with
    /// `ExternalToolPending`. The agent message stays in `Executing` until
    /// the human resolves and the per-message barrier clears.
    ///
    /// Mutually exclusive with `with_task_event` - the last builder called
    /// wins, but `debug_assert` catches the contradiction in debug builds.
    pub fn with_hitl(mut self, h: crate::inference::hitl::Hitl) -> Self {
        debug_assert!(
            self.task_event.is_none(),
            "ToolOutput::with_hitl called after with_task_event — these are mutually exclusive"
        );
        self.hitl = Some(h);
        self
    }

    /// Attach a terminal task-control signal. The tool loop detects this and
    /// exits as `Completed` with the event passed up as `lifecycle_event`.
    pub fn with_task_event(mut self, e: crate::inference::tool_call::TaskEvent) -> Self {
        debug_assert!(
            self.hitl.is_none(),
            "ToolOutput::with_task_event called after with_hitl — these are mutually exclusive"
        );
        self.task_event = Some(e);
        self
    }

    pub fn with_system_prompt(mut self, s: impl Into<String>) -> Self {
        self.system_prompt = Some(s.into());
        self
    }

    pub fn text_content(&self) -> &str {
        &self.text
    }

    pub fn images(&self) -> &[ImageData] {
        &self.images
    }

    pub fn attachments(&self) -> &[crate::storage::Attachment] {
        &self.attachments
    }

    pub fn hitl(&self) -> Option<&crate::inference::hitl::Hitl> {
        self.hitl.as_ref()
    }

    pub fn take_hitl(&mut self) -> Option<crate::inference::hitl::Hitl> {
        self.hitl.take()
    }

    pub fn task_event(&self) -> Option<&crate::inference::tool_call::TaskEvent> {
        self.task_event.as_ref()
    }

    pub fn take_task_event(&mut self) -> Option<crate::inference::tool_call::TaskEvent> {
        self.task_event.take()
    }

    pub fn as_pending_external(mut self) -> Self {
        self.pending_external = true;
        self
    }

    pub fn is_pending_external(&self) -> bool {
        self.pending_external
    }

    pub fn is_success(&self) -> bool {
        self.success
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }
}

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn definition_vars(&self) -> Vec<(&str, &str)> {
        vec![]
    }
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError>;
    /// Called after a human resolves a HITL prompt this tool emitted from
    /// `execute`. The tool reads its original `request` payload, validates the
    /// `response` shape, performs any side effect (deploy, bind credential,
    /// etc.), and returns the result text that gets persisted as
    /// `te.result` - what the LLM reads in conversation history on resume.
    ///
    /// Default returns an error. Tools that emit HITLs must override.
    async fn on_resume(
        &self,
        tool_name: &str,
        request: &crate::inference::hitl::HitlRequest,
        response: crate::inference::hitl::HitlResponse,
        ctx: &InferenceContext,
    ) -> Result<crate::inference::hitl::HitlOutcome, AppError> {
        let _ = (request, response, ctx);
        Err(AppError::Validation(format!(
            "tool '{tool_name}' does not implement on_resume — cannot resolve HITL"
        )))
    }
    async fn cleanup(&self) -> Result<(), AppError> {
        Ok(())
    }
}

fn parse_frontmatter(raw: &str) -> Option<(Value, String)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = &trimmed[3..];
    let end = after_first.find("---")?;
    let yaml_str = &after_first[..end];
    let body = after_first[end + 3..].trim().to_string();
    let yaml: Value = serde_yaml::from_str(yaml_str).ok()?;
    Some((yaml, body))
}

fn build_parameters_json(yaml: &Value) -> Value {
    let params = yaml.get("parameters").cloned().unwrap_or(Value::Null);
    let required = yaml.get("required").cloned().unwrap_or(Value::Null);

    let properties: Value = if let Value::Object(map) = &params {
        let mut props = serde_json::Map::new();
        for (key, schema) in map {
            props.insert(
                key.clone(),
                serde_json::to_value(schema).unwrap_or(Value::Null),
            );
        }
        Value::Object(props)
    } else {
        Value::Object(serde_json::Map::new())
    };

    let mut result = serde_json::json!({
        "type": "object",
        "properties": properties,
    });

    if let Value::Array(arr) = &required {
        let req: Vec<Value> = arr
            .iter()
            .map(|v| {
                if let Value::String(s) = v {
                    Value::String(s.clone())
                } else {
                    v.clone()
                }
            })
            .collect();
        result["required"] = Value::Array(req);
    }

    result
}

pub fn load_tool_definition(prompts: &PromptLoader, path: &str) -> Option<ToolDefinition> {
    load_tool_definition_with_vars(prompts, path, &[])
}

pub fn load_tool_definition_with_vars(
    prompts: &PromptLoader,
    path: &str,
    vars: &[(&str, &str)],
) -> Option<ToolDefinition> {
    let raw = prompts.read_with_vars(path, vars)?;
    let (yaml, body) = parse_frontmatter(&raw)?;
    let id = yaml.get("id")?.as_str()?.to_string();
    let provider_id = yaml
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let parameters = build_parameters_json(&yaml);
    Some(ToolDefinition {
        id,
        provider_id,
        description: body,
        parameters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_run_at_unix_timestamp_number() {
        let dt = parse_run_at(&json!(4_000_000_000_i64), "UTC")
            .unwrap()
            .unwrap();
        assert_eq!(dt.timestamp(), 4_000_000_000);
    }

    #[test]
    fn parse_run_at_unix_timestamp_string() {
        let dt = parse_run_at(&json!("4000000000"), "UTC").unwrap().unwrap();
        assert_eq!(dt.timestamp(), 4_000_000_000);
    }

    #[test]
    fn parse_run_at_naive_in_la_winter() {
        // 22:00 on 2030-01-15 in LA (PST = UTC-8) → 06:00 UTC on 01-16
        let dt = parse_run_at(&json!("2030-01-15T22:00:00"), "America/Los_Angeles")
            .unwrap()
            .unwrap();
        assert_eq!(dt.to_rfc3339(), "2030-01-16T06:00:00+00:00");
    }

    #[test]
    fn parse_run_at_naive_in_la_summer() {
        // 22:00 on 2030-07-15 in LA (PDT = UTC-7) → 05:00 UTC on 07-16
        let dt = parse_run_at(&json!("2030-07-15T22:00:00"), "America/Los_Angeles")
            .unwrap()
            .unwrap();
        assert_eq!(dt.to_rfc3339(), "2030-07-16T05:00:00+00:00");
    }

    #[test]
    fn parse_run_at_naive_in_tokyo() {
        // 06:00 on 2030-05-20 in Tokyo (JST = UTC+9) → 21:00 UTC on 05-19
        let dt = parse_run_at(&json!("2030-05-20T06:00:00"), "Asia/Tokyo")
            .unwrap()
            .unwrap();
        assert_eq!(dt.to_rfc3339(), "2030-05-19T21:00:00+00:00");
    }

    #[test]
    fn parse_run_at_rejects_explicit_offset() {
        let err = parse_run_at(&json!("2030-05-20T22:00:00-04:00"), "UTC").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("explicit UTC offset"),
            "expected hint about explicit offset, got: {msg}"
        );
    }

    #[test]
    fn parse_run_at_rejects_z_suffix() {
        let err = parse_run_at(&json!("2030-05-20T22:00:00Z"), "UTC").unwrap_err();
        assert!(err.to_string().contains("explicit UTC offset"));
    }

    #[test]
    fn parse_run_at_rejects_past_time() {
        let err = parse_run_at(&json!("2000-01-01T00:00:00"), "UTC").unwrap_err();
        assert!(err.to_string().contains("future"));
    }

    #[test]
    fn parse_run_at_invalid_tz_rejected() {
        let err = parse_run_at(&json!("2030-05-20T22:00:00"), "Mars/Olympus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Invalid timezone"), "got: {msg}");
    }

    #[test]
    fn parse_run_at_accepts_space_separator() {
        let dt = parse_run_at(&json!("2030-05-20 22:00:00"), "UTC")
            .unwrap()
            .unwrap();
        assert_eq!(dt.to_rfc3339(), "2030-05-20T22:00:00+00:00");
    }

    #[test]
    fn resolve_run_at_delay_minutes_takes_precedence() {
        let args = json!({"delay_minutes": 5, "run_at": "2030-05-20T22:00:00"});
        let dt = resolve_run_at(&args, "UTC").unwrap().unwrap();
        // Should be ~5min from now, not 2030 - delay_minutes wins.
        let delta = (dt - chrono::Utc::now()).num_minutes();
        assert!(
            (4..=6).contains(&delta),
            "expected ~5 min from now, got {delta}"
        );
    }

    #[test]
    fn resolve_run_at_delay_minutes_zero_rejected() {
        let args = json!({"delay_minutes": 0});
        assert!(resolve_run_at(&args, "UTC").is_err());
    }
}
