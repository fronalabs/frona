use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::evidence::ToolExecution;

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
    pub evidence: super::evidence::ToolEvidenceProjection,
}

impl RecallProjection {
    pub fn new(calls: &[ToolExecution], permitted_page_read: impl Fn(&str) -> bool) -> Self {
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
                result: frona_text::redaction::redact_text(&call.result),
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

    pub fn render_for_message(&self, message_id: &str, handle: &str) -> String {
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

    pub fn with_evidence(mut self, evidence: super::evidence::ToolEvidenceProjection) -> Self {
        self.evidence = evidence;
        self
    }
}

fn recall_arguments(call: &ToolExecution) -> Value {
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

fn bounded_result_tokens(input: &str, cap: usize) -> String {
    if frona_harness::inference::context::estimate_tokens(input) <= cap {
        return input.to_string();
    }
    let mut out = String::new();
    for line in input.lines() {
        let candidate = if out.is_empty() {
            line.to_string()
        } else {
            format!("{out}\n{line}")
        };
        if frona_harness::inference::context::estimate_tokens(&format!("{candidate}…")) > cap {
            break;
        }
        out = candidate;
    }
    if out.is_empty() {
        for character in input.chars() {
            let mut candidate = out.clone();
            candidate.push(character);
            candidate.push('…');
            if frona_harness::inference::context::estimate_tokens(&candidate) > cap {
                break;
            }
            out.push(character);
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests;
