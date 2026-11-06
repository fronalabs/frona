use frona_harness::{Handle, retry_config::RetryConfig};
use serde_json::json;

fn retry_json() -> serde_json::Value {
    json!({
        "max_retries": 4,
        "initial_backoff_ms": 250,
        "backoff_multiplier": 1.5,
        "max_backoff_ms": 5000
    })
}

#[test]
fn portable_handle_keeps_its_validated_string_representation() {
    let handle: Handle = serde_json::from_value(json!("  Alpha-Beta  ")).unwrap();
    assert_eq!(serde_json::to_value(&handle).unwrap(), json!("alpha-beta"));
    assert!(serde_json::from_value::<Handle>(json!("invalid.handle")).is_err());
    let schema = schemars::schema_for!(Handle).to_value();
    assert_eq!(schema["pattern"], "^[a-z][a-z0-9_-]{1,31}$");
    assert_eq!(schema["minLength"], 2);
    assert_eq!(schema["maxLength"], 32);
}

#[test]
fn portable_configuration_keeps_fields_defaults_and_schema() {
    let retry: RetryConfig = serde_json::from_value(retry_json()).unwrap();
    assert_eq!(serde_json::to_value(retry).unwrap(), retry_json());
    assert_eq!(
        serde_json::from_value::<RetryConfig>(json!({})).unwrap(),
        RetryConfig::default()
    );
    assert!(
        schemars::schema_for!(RetryConfig).to_value()["properties"]
            .get("backoff_multiplier")
            .is_some()
    );
}

#[test]
fn portable_errors_preserve_typed_execution_stops() {
    use frona_harness::{BudgetRefusal, ExecutionStop, inference::InferenceError};
    let error: InferenceError =
        ExecutionStop::BudgetExhausted(BudgetRefusal::Denied("application policy".into())).into();
    assert!(
        matches!(error.into_execution_stop(), Ok(ExecutionStop::BudgetExhausted(BudgetRefusal::Denied(reason))) if reason == "application policy")
    );
}
