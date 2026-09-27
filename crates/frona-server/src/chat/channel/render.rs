//! TaskCompletion rows with a schema carry raw JSON in `content`; this
//! module re-renders that JSON to markdown. Adapters then apply their
//! native converter (`markdown::to_signal`, `to_whatsapp`, `to_plain`).

use serde_json::Value;

use crate::chat::message::models::{Message, MessageEvent};

/// Top-level object is "complex" when any of its values is itself an object
/// or contains objects (array of objects). Complex schemas must include a
/// top-level `summary` string property; the renderer surfaces only that
/// field, treating everything else as machine-readable for the parent agent.
const COMPLEX_RENDER_KEY: &str = "summary";

pub fn render_message_body(msg: &Message) -> String {
    let Some(MessageEvent::TaskCompletion {
        schema: Some(schema),
        ..
    }) = &msg.event
    else {
        return msg.content.clone();
    };
    let parsed: Value = match serde_json::from_str(&msg.content) {
        Ok(v) => v,
        Err(_) => return msg.content.clone(),
    };
    render_result_markdown(schema, &parsed).unwrap_or_else(|| msg.content.clone())
}

/// Returns `None` for null / empty obj / empty array — i.e. no delivery.
pub fn render_result_markdown(schema: &Value, value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    match value {
        Value::Array(arr) => {
            if arr.is_empty() {
                return None;
            }
            Some(
                arr.iter()
                    .map(|v| format!("- {}", render_value_md(v)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                return None;
            }
            if is_complex_object(obj) {
                return render_complex_object(schema, obj);
            }
            let props = find_object_properties(schema);
            let mut lines: Vec<(String, String)> = Vec::new();
            if let Some(props) = props {
                for (key, prop_schema) in props {
                    match obj.get(key) {
                        Some(v) if !v.is_null() => {
                            let label = prop_schema
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or(key.as_str())
                                .to_string();
                            lines.push((label, render_value_md(v)));
                        }
                        _ => {}
                    }
                }
            } else {
                for (key, v) in obj {
                    if !v.is_null() {
                        lines.push((key.clone(), render_value_md(v)));
                    }
                }
            }
            if lines.is_empty() {
                return None;
            }
            if lines.len() == 1 {
                return Some(lines.remove(0).1);
            }
            Some(
                lines
                    .iter()
                    .map(|(label, val)| format!("**{label}**: {val}"))
                    .collect::<Vec<_>>()
                    // Single newlines are Markdown soft breaks and collapse
                    // to spaces in channel converters such as Signal's.
                    .join("\n\n"),
            )
        }
        _ => Some(render_value_md(value)),
    }
}

fn is_complex_object(obj: &serde_json::Map<String, Value>) -> bool {
    obj.values().any(value_is_non_scalar)
}

fn value_is_non_scalar(v: &Value) -> bool {
    match v {
        Value::Object(_) => true,
        Value::Array(arr) => arr.iter().any(value_is_non_scalar),
        _ => false,
    }
}

fn render_complex_object(_schema: &Value, obj: &serde_json::Map<String, Value>) -> Option<String> {
    match obj.get(COMPLEX_RENDER_KEY) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn find_object_properties(schema: &Value) -> Option<&serde_json::Map<String, Value>> {
    if let Some(p) = schema.get("properties").and_then(|v| v.as_object()) {
        return Some(p);
    }
    if let Some(Value::Array(branches)) = schema.get("oneOf").or_else(|| schema.get("anyOf")) {
        for branch in branches {
            if let Some(p) = branch.get("properties").and_then(|v| v.as_object()) {
                return Some(p);
            }
        }
    }
    None
}

fn render_value_md(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Array(arr) => arr
            .iter()
            .map(render_value_md)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(_) => format!(
            "```json\n{}\n```",
            serde_json::to_string_pretty(v).unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::task::models::TaskStatus;
    use crate::chat::message::models::{Message, MessageRole};
    use serde_json::json;

    fn task_completion(content: &str, schema: Option<Value>) -> Message {
        let mut msg =
            Message::builder("c1", MessageRole::TaskCompletion, content.to_string()).build();
        msg.event = Some(MessageEvent::TaskCompletion {
            task_id: "t1".into(),
            chat_id: None,
            status: TaskStatus::Completed,
            summary: Some(content.to_string()),
            schema,
        });
        msg
    }

    #[test]
    fn plain_agent_message_passes_through() {
        let msg = Message::builder("c1", MessageRole::Agent, "hello".into()).build();
        assert_eq!(render_message_body(&msg), "hello");
    }

    #[test]
    fn task_completion_without_schema_passes_content_through() {
        let msg = task_completion("raw text", None);
        assert_eq!(render_message_body(&msg), "raw text");
    }

    #[test]
    fn task_completion_with_schema_renders_markdown() {
        let schema = json!({
            "type": "object",
            "properties": {
                "symbol": {"type": "string", "description": "ticker"},
                "price": {"type": "number", "description": "current price (USD)"}
            }
        });
        let msg = task_completion(r#"{"symbol":"AAPL","price":234}"#, Some(schema));
        assert_eq!(
            render_message_body(&msg),
            "**ticker**: AAPL\n\n**current price (USD)**: 234"
        );
    }

    #[test]
    fn task_completion_fields_remain_separate_paragraphs_in_signal() {
        let schema = json!({
            "type": "object",
            "properties": {
                "amazon": {"type": "string", "description": "Amazon news summary with stock price"},
                "anthropic": {"type": "string", "description": "Anthropic news summary"}
            }
        });
        let msg = task_completion(
            r#"{"amazon":"AMZN $246.53.","anthropic":"Private/no ticker."}"#,
            Some(schema),
        );
        let markdown = render_message_body(&msg);
        let signal = crate::chat::channel::adapter::markdown::to_signal(&markdown);

        assert_eq!(
            signal.body,
            "Amazon news summary with stock price: AMZN $246.53.\n\nAnthropic news summary: Private/no ticker."
        );
    }

    #[test]
    fn task_completion_parse_failure_falls_back_to_content() {
        let schema = json!({"type": "object"});
        let msg = task_completion("not json", Some(schema));
        assert_eq!(render_message_body(&msg), "not json");
    }

    #[test]
    fn render_scalar_string() {
        let schema = json!({"type": "string"});
        assert_eq!(
            render_result_markdown(&schema, &json!("hello")),
            Some("hello".to_string())
        );
    }

    #[test]
    fn render_array_of_scalars_bullet_list() {
        let schema = json!({"type": "array", "items": {"type": "string"}});
        assert_eq!(
            render_result_markdown(&schema, &json!(["a", "b", "c"])),
            Some("- a\n- b\n- c".to_string())
        );
    }

    #[test]
    fn render_object_multi_prop_uses_bold_descriptions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "symbol": {"type": "string", "description": "ticker"},
                "price": {"type": "number", "description": "price"}
            }
        });
        let value = json!({"symbol": "AAPL", "price": 234});
        assert_eq!(
            render_result_markdown(&schema, &value),
            Some("**ticker**: AAPL\n\n**price**: 234".to_string())
        );
    }

    #[test]
    fn render_object_single_prop_renders_bare_value() {
        let schema = json!({
            "type": "object",
            "properties": {"joke": {"type": "string", "description": "the joke"}}
        });
        let value = json!({"joke": "Why did the chicken cross the road?"});
        assert_eq!(
            render_result_markdown(&schema, &value),
            Some("Why did the chicken cross the road?".to_string())
        );
    }

    #[test]
    fn render_null_value_returns_none() {
        let schema = json!({"type": ["string", "null"]});
        assert_eq!(render_result_markdown(&schema, &Value::Null), None);
    }

    #[test]
    fn render_empty_array_returns_none() {
        let schema = json!({"type": "array", "items": {"type": "string"}});
        assert_eq!(render_result_markdown(&schema, &json!([])), None);
    }

    #[test]
    fn complex_object_renders_only_summary_field() {
        let schema = json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string"},
                "phones": {"type": "array", "items": {"type": "object"}},
                "recommendation": {"type": "string"}
            }
        });
        let value = json!({
            "summary": "Overall context",
            "phones": [{"name": "X"}, {"name": "Y"}],
            "recommendation": "Buy X"
        });
        assert_eq!(
            render_result_markdown(&schema, &value),
            Some("Overall context".to_string())
        );
    }

    #[test]
    fn complex_object_with_no_summary_returns_none() {
        let schema = json!({
            "type": "object",
            "properties": {
                "phones": {"type": "array", "items": {"type": "object"}},
                "result": {"type": "string"}
            }
        });
        let value = json!({
            "phones": [{"name": "X"}],
            "result": "previously-recognized field, now ignored"
        });
        assert_eq!(render_result_markdown(&schema, &value), None);
    }
}
