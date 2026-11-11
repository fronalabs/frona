use super::*;
use serde_json::json;

#[test]
fn field_actions_apply_recursively_without_changing_other_values() {
    let input = json!({"nested": [{"password": {"raw": "secret"}, "binary": "payload", "port": 8080}], "enabled": true, "unset": null, "text": "plain text"});
    let output = redact_json(&input, |key| match key {
        "password" => FieldAction::Redact,
        "binary" => FieldAction::Omit,
        _ => FieldAction::Keep,
    });
    assert_eq!(
        output,
        json!({"nested": [{"password": "[REDACTED]", "port": 8080}], "enabled": true, "unset": null, "text": "plain text"})
    );
    assert_eq!(input["nested"][0]["binary"], "payload");
}

#[test]
fn retained_strings_and_array_elements_use_the_text_scanner() {
    let secret = format!("ghp_{}", "a".repeat(36));
    let text = format!("credential {secret}");
    let redacted = redact_text(&text);
    assert!(!redacted.contains(&secret), "{redacted}");
    let input = json!({"note": [text, {"value": text}]});
    assert_eq!(
        redact_json(&input, |_| FieldAction::Keep),
        json!({"note": [redacted, {"value": redacted}]})
    );
    assert_eq!(
        redact_json(&json!(text), |_| FieldAction::Keep),
        json!(redacted)
    );
}
