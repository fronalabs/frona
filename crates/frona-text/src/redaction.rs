//! Best-effort secret redaction for text and JSON. Callers choose field policies.
use serde_json::Value;

/// How to project an object field. Retained strings are still scanned for secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldAction {
    Keep,
    Omit,
    Redact,
}

/// Replace automatically recognized secrets in text.
pub fn redact_text(input: &str) -> String {
    rtb_redact::string(input).into_owned()
}

/// Recursively redact string values and apply the caller's policy to object keys.
/// Redacted fields retain their key with a `[REDACTED]` value; omitted fields are
/// removed. Arrays and non-string scalar values otherwise retain their shape.
pub fn redact_json(value: &Value, field_action: impl Fn(&str) -> FieldAction) -> Value {
    fn visit(value: &Value, action: &impl Fn(&str) -> FieldAction) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter_map(|(key, value)| {
                        let value = match action(key) {
                            FieldAction::Keep => visit(value, action),
                            FieldAction::Omit => return None,
                            FieldAction::Redact => Value::String("[REDACTED]".into()),
                        };
                        Some((key.clone(), value))
                    })
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.iter().map(|v| visit(v, action)).collect()),
            Value::String(value) => Value::String(redact_text(value)),
            value => value.clone(),
        }
    }
    visit(value, &field_action)
}

#[cfg(test)]
mod tests;
