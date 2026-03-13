use chrono::{Datelike, Duration, Months, SecondsFormat, Utc};
use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use frona_derive::agent_tool;

use super::{InferenceContext, ToolOutput};

pub struct TimeTool {
    prompts: PromptLoader,
}

impl TimeTool {
    pub fn new(prompts: PromptLoader) -> Self {
        Self { prompts }
    }
}

#[agent_tool(files("get_time"))]
impl TimeTool {
    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
        let mut dt = Utc::now();

        if let Some(minutes) = arguments.get("add_minutes").and_then(|v| v.as_i64()) {
            dt += Duration::minutes(minutes);
        }
        if let Some(hours) = arguments.get("add_hours").and_then(|v| v.as_i64()) {
            dt += Duration::hours(hours);
        }
        if let Some(days) = arguments.get("add_days").and_then(|v| v.as_i64()) {
            dt += Duration::days(days);
        }
        if let Some(weeks) = arguments.get("add_weeks").and_then(|v| v.as_i64()) {
            dt += Duration::weeks(weeks);
        }
        if let Some(months) = arguments.get("add_months").and_then(|v| v.as_i64()) {
            if months >= 0 {
                dt = dt
                    .checked_add_months(Months::new(months as u32))
                    .ok_or_else(|| AppError::Validation("Month offset out of range".into()))?;
            } else {
                dt = dt
                    .checked_sub_months(Months::new(months.unsigned_abs() as u32))
                    .ok_or_else(|| AppError::Validation("Month offset out of range".into()))?;
            }
        }

        let weekday = dt.weekday();
        let result = serde_json::json!({
            "utc": dt.to_rfc3339_opts(SecondsFormat::Secs, true),
            "unix_timestamp": dt.timestamp(),
            "human_readable": dt.format("%A, %B %e, %Y %H:%M:%S UTC").to_string(),
            "weekday": weekday.to_string(),
        });

        Ok(ToolOutput::text(result.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{AgentTool, InferenceContext};

    fn mock_context() -> InferenceContext {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        InferenceContext::new(
            crate::auth::User {
                id: "u".into(), username: "u".into(), email: "e".into(), name: "n".into(),
                password_hash: String::new(),
                created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
            },
            crate::agent::models::Agent {
                id: "a".into(), user_id: None, name: "a".into(),
                description: String::new(), model_group: "p".into(), enabled: true,
                tools: vec![], sandbox_config: None, max_concurrent_tasks: None,
                avatar: None, identity: Default::default(),
                heartbeat_interval: None, next_heartbeat_at: None,
                heartbeat_chat_id: None,
                created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
            },
            crate::chat::models::Chat {
                id: "c".into(), user_id: "u".into(), space_id: None,
                task_id: None, agent_id: "a".into(), title: None,
                archived_at: None,
                created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(),
            },
            tx,
        )
    }

    #[tokio::test]
    async fn utc_format_uses_z_suffix_without_subseconds() {
        let tool = TimeTool::new(PromptLoader::new("/nonexistent"));
        let ctx = mock_context();
        let result = tool.execute("get_time", serde_json::json!({}), &ctx).await.unwrap();
        let parsed: Value = serde_json::from_str(result.text_content()).unwrap();
        let utc = parsed["utc"].as_str().unwrap();

        assert!(utc.ends_with('Z'), "Expected Z suffix, got: {utc}");
        assert!(
            !utc.contains('.'),
            "Expected no sub-second precision, got: {utc}"
        );
        utc.parse::<chrono::DateTime<Utc>>().expect("Should parse as DateTime<Utc>");
    }
}
