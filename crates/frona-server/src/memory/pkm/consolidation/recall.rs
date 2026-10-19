use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};

use frona_derive::agent_tool;
use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::inference::tool_call::ToolCall;
use crate::tool::{InferenceContext, ToolOutput, str_arg};

pub(crate) const RECALL_RESULT_TOKEN_CAP: usize = 4_000;

#[derive(Debug, Clone)]
pub(crate) struct ProjectedRecallCall {
    pub local_id: String,
    pub name: String,
    pub arguments: Value,
    pub result: String,
}

#[derive(Debug, Clone, Default)]
pub struct RecallProjection {
    by_message: BTreeMap<String, Vec<ProjectedRecallCall>>,
    by_local_id: HashMap<String, ProjectedRecallCall>,
    pub(crate) evidence: super::evidence::ToolEvidenceProjection,
}

impl RecallProjection {
    pub(crate) fn new(calls: &[ToolCall], permitted_page_read: impl Fn(&str) -> bool) -> Self {
        let mut eligible = calls
            .iter()
            .filter(|call| {
                if !call.success {
                    return false;
                }
                match call.name.as_str() {
                    "memory_search" => true,
                    "read" => call
                        .arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .is_some_and(&permitted_page_read),
                    _ => false,
                }
            })
            .collect::<Vec<_>>();
        eligible.sort_by(|a, b| {
            (&a.message_id, a.turn, a.created_at, &a.id).cmp(&(
                &b.message_id,
                b.turn,
                b.created_at,
                &b.id,
            ))
        });

        let mut projection = Self::default();
        for (index, call) in eligible.into_iter().enumerate() {
            let projected = ProjectedRecallCall {
                local_id: format!("T{}", index + 1),
                name: call.name.clone(),
                arguments: recall_arguments(call),
                result: sanitize(&call.result),
            };
            projection
                .by_message
                .entry(call.message_id.clone())
                .or_default()
                .push(projected.clone());
            projection
                .by_local_id
                .insert(projected.local_id.clone(), projected);
        }
        projection
    }

    pub(crate) fn for_message(&self, message_id: &str) -> &[ProjectedRecallCall] {
        self.by_message
            .get(message_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn render_for_message(&self, message_id: &str, handle: &str) -> String {
        let calls = self.for_message(message_id);
        if calls.is_empty() {
            return String::new();
        }
        let mut rendered = format!(
            "Recall calls for {handle} (prior retrieval; decision context only, never cite T-ids):\n"
        );
        for call in calls {
            let (kind, value) = match call.name.as_str() {
                "memory_search" => ("keyword search", call.arguments.get("query")),
                "read" => ("page path", call.arguments.get("path")),
                _ => ("recall", None),
            };
            let value = value.and_then(Value::as_str).unwrap_or("(unknown)");
            rendered.push_str(&format!("- [{}] {} — {}\n", call.local_id, kind, value));
        }
        rendered
    }

    pub(crate) fn read_result(&self, local_id: &str) -> Option<String> {
        self.by_local_id
            .get(local_id)
            .map(|call| bounded_result_tokens(&call.result, RECALL_RESULT_TOKEN_CAP))
    }

    pub(crate) fn result_calls_for_message(&self, message_id: &str) -> &[ProjectedRecallCall] {
        self.for_message(message_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_local_id.len()
    }

    pub(crate) fn preview_chars(&self) -> usize {
        0
    }

    pub(crate) fn with_evidence(
        mut self,
        evidence: super::evidence::ToolEvidenceProjection,
    ) -> Self {
        self.evidence = evidence;
        self
    }
}

pub(crate) struct ReadRecallResultTool {
    pub prompts: PromptLoader,
    pub projection: RecallProjection,
    pub lookups: std::sync::Arc<AtomicUsize>,
}

#[agent_tool(name = "read_recall_result", dir = "pkm")]
impl ReadRecallResultTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let Some(call_id) = str_arg(&arguments, "call_id") else {
            return Ok(ToolOutput::text(
                "Provide a recall call_id shown in this extraction window.",
            ));
        };
        let Some(result) = self.projection.read_result(call_id) else {
            return Ok(ToolOutput::text(
                "Unknown recall call_id. Only T-ids shown in this extraction window are readable.",
            ));
        };
        self.lookups.fetch_add(1, Ordering::Relaxed);
        Ok(ToolOutput::text(result))
    }
}

fn recall_arguments(call: &ToolCall) -> Value {
    let mut kept = serde_json::Map::new();
    match call.name.as_str() {
        "memory_search" => {
            if let Some(query) = call.arguments.get("query") {
                kept.insert("query".into(), query.clone());
            }
        }
        "read" => {
            if let Some(path) = call.arguments.get("path") {
                kept.insert("path".into(), path.clone());
            }
        }
        _ => {}
    }
    Value::Object(kept)
}

fn sanitize(input: &str) -> String {
    rtb_redact::string(input).into_owned()
}

fn bounded_result_tokens(input: &str, cap: usize) -> String {
    if crate::inference::context::estimate_tokens(input) <= cap {
        return input.to_string();
    }
    let mut out = String::new();
    for line in input.lines() {
        let candidate = if out.is_empty() {
            line.to_string()
        } else {
            format!("{out}\n{line}")
        };
        if crate::inference::context::estimate_tokens(&format!("{candidate}…")) > cap {
            break;
        }
        out = candidate;
    }
    if out.is_empty() {
        for character in input.chars() {
            let mut candidate = out.clone();
            candidate.push(character);
            candidate.push('…');
            if crate::inference::context::estimate_tokens(&candidate) > cap {
                break;
            }
            out.push(character);
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use crate::inference::tool_call::ToolCall;

    fn call(
        id: &str,
        message_id: &str,
        turn: u32,
        name: &str,
        arguments: serde_json::Value,
        result: &str,
        success: bool,
    ) -> ToolCall {
        ToolCall {
            id: id.into(),
            chat_id: "chat-1".into(),
            message_id: message_id.into(),
            turn,
            provider_call_id: format!("provider-{id}"),
            name: name.into(),
            arguments,
            result: result.into(),
            success,
            duration_ms: 1,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            signature: None,
            turn_reasoning: None,
            created_at: Utc.timestamp_opt(i64::from(turn), 0).unwrap(),
        }
    }

    #[test]
    fn projection_keeps_only_successful_recall_calls_in_message_order() {
        let calls = vec![
            call(
                "search-2",
                "agent-1",
                2,
                "memory_search",
                json!({"query":"phone"}),
                "Casey Owner — phone 555-0100",
                true,
            ),
            call(
                "shell",
                "agent-1",
                1,
                "shell",
                json!({"command":"pwd"}),
                "/tmp",
                true,
            ),
            call(
                "failed",
                "agent-1",
                3,
                "memory_search",
                json!({"query":"boss"}),
                "failed",
                false,
            ),
            call(
                "search-1",
                "agent-1",
                1,
                "memory_search",
                json!({"query":"Casey Owner"}),
                "Casey Owner — spouse",
                true,
            ),
        ];

        let projection = RecallProjection::new(&calls, |_| false);
        let projected = projection.for_message("agent-1");
        assert_eq!(
            projected
                .iter()
                .map(|call| call.local_id.as_str())
                .collect::<Vec<_>>(),
            vec!["T1", "T2"]
        );
        assert_eq!(
            projected
                .iter()
                .map(|call| call.arguments["query"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["Casey Owner", "phone"]
        );
    }

    #[test]
    fn projection_accepts_only_reads_inside_the_knowledge_vault() {
        let calls = vec![
            call(
                "page",
                "agent-1",
                1,
                "read",
                json!({"path":"/vault/Memory/people/casey-owner.md"}),
                "# Casey Owner",
                true,
            ),
            call(
                "secret",
                "agent-1",
                2,
                "read",
                json!({"path":"/vault/secrets.txt"}),
                "token",
                true,
            ),
        ];

        let projection = RecallProjection::new(&calls, |path| path.starts_with("/vault/Memory/"));
        let projected = projection.for_message("agent-1");
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].local_id, "T1");
        assert!(
            !projection
                .render_for_message("agent-1", "m2")
                .contains("token")
        );
    }

    #[test]
    fn stored_result_lookup_is_scoped_and_bounded() {
        let calls = vec![call(
            "page",
            "agent-1",
            1,
            "read",
            json!({"path":"/vault/Memory/people/casey-owner.md"}),
            &"line\n".repeat(5_000),
            true,
        )];
        let projection = RecallProjection::new(&calls, |_| true);

        assert!(
            crate::inference::context::estimate_tokens(&projection.read_result("T1").unwrap())
                <= RECALL_RESULT_TOKEN_CAP
        );
        assert!(projection.read_result("T999").is_none());
    }
}
