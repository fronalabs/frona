use super::*;
use serde_json::json;

#[test]
fn projections_preserve_their_distinct_field_policies_recursively() {
    let input = json!({"PASSWORD": "secret", "nested": [{"api_key": "secret", "binary": "payload", "client-credential": "custom", "count": 3}], "ok": true});
    assert_eq!(
        sanitize_arguments(&input),
        json!({"nested": [{"client-credential": "custom", "count": 3}], "ok": true})
    );
    assert_eq!(
        redact_json(&input),
        json!({"PASSWORD": "[REDACTED]", "nested": [{"api_key": "[REDACTED]", "binary": "payload", "client-credential": "[REDACTED]", "count": 3}], "ok": true})
    );
}
