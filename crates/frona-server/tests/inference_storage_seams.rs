use frona::core::{Handle, config::RetryConfig};
use frona::inference::{
    handle::Handle as ProviderHandle, retry_config::RetryConfig as RetryPolicy,
};
use serde_json::json;
use surrealdb::types::{Kind, SurrealValue};

fn retry_fixture() -> serde_json::Value {
    json!({
        "max_retries": 4,
        "initial_backoff_ms": 250,
        "backoff_multiplier": 1.5,
        "max_backoff_ms": 5000
    })
}

#[test]
fn stored_handle_keeps_native_shape_and_shared_validation() {
    let stored = Handle::from_value("  Alpha-Beta  ".to_string().into_value()).unwrap();
    let portable: ProviderHandle = stored.clone().into();
    assert_eq!(portable.as_str(), "alpha-beta");
    assert_eq!(Handle::from(portable.clone()), stored);
    assert_eq!(
        stored.clone().into_value().into_json_value(),
        json!("alpha-beta")
    );
    assert_eq!(Handle::kind_of(), Kind::String);
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        serde_json::to_value(&portable).unwrap()
    );
    assert_eq!(
        schemars::schema_for!(Handle).to_value(),
        schemars::schema_for!(ProviderHandle).to_value()
    );
    for invalid in ["a", "9alice", "invalid.handle", "alice/bob", "", "éclair"] {
        assert!(Handle::from_value(invalid.to_string().into_value()).is_err());
        assert!(Handle::try_new(invalid).is_err());
        assert!(ProviderHandle::try_new(invalid).is_err());
    }
    assert!(Handle::from_value(42_i64.into_value()).is_err());
}

#[test]
fn stored_retry_shape_converts_to_the_same_execution_policy() {
    let stored = RetryConfig::from_value(retry_fixture().into_value()).unwrap();
    let portable = RetryPolicy::from(&stored);
    assert_eq!(stored.into_value().into_json_value(), retry_fixture());
    assert_eq!(serde_json::to_value(portable).unwrap(), retry_fixture());
    assert_eq!(
        RetryPolicy::from(&RetryConfig::default()),
        RetryPolicy::default()
    );
    assert_eq!(
        schemars::schema_for!(RetryConfig).to_value(),
        schemars::schema_for!(RetryPolicy).to_value()
    );
}

#[test]
fn native_retry_rejects_missing_or_mistyped_fields_while_serde_keeps_defaults() {
    // Native records require the complete shape; serde's container default is
    // intentionally a different input contract. Do not replace it with a serde
    // field codec when the execution policy moves to another crate.
    for field in [
        "max_retries",
        "initial_backoff_ms",
        "backoff_multiplier",
        "max_backoff_ms",
    ] {
        let mut missing = retry_fixture();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            RetryConfig::from_value(missing.into_value()).is_err(),
            "{field}"
        );
        let mut wrong_type = retry_fixture();
        wrong_type[field] = json!("invalid");
        assert!(
            RetryConfig::from_value(wrong_type.into_value()).is_err(),
            "{field}"
        );
    }
    let mut negative = retry_fixture();
    negative["max_retries"] = json!(-1);
    assert!(RetryConfig::from_value(negative.into_value()).is_err());
    assert_eq!(
        serde_json::from_value::<RetryConfig>(json!({})).unwrap(),
        RetryConfig::default()
    );
    assert_eq!(
        serde_json::from_value::<RetryPolicy>(json!({})).unwrap(),
        RetryPolicy::default()
    );
}

#[test]
fn retained_server_limits_keep_native_and_serde_contracts() {
    use frona::core::config::SandboxLimits;
    let fixture = json!({"max_cpu_pct": 75.0, "max_memory_pct": 60.0, "timeout_secs": 30});
    let native = SandboxLimits::from_value(fixture.clone().into_value()).unwrap();
    assert_eq!(native.into_value().into_json_value(), fixture);
    let serde: SandboxLimits = serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(serde_json::to_value(serde).unwrap(), fixture);
    assert_eq!(
        serde_json::from_value::<SandboxLimits>(json!({"timeout_secs": 15})).unwrap(),
        SandboxLimits {
            timeout_secs: 15,
            ..SandboxLimits::default()
        }
    );
    assert!(
        schemars::schema_for!(SandboxLimits).to_value()["properties"]
            .get("timeout_secs")
            .is_some()
    );
}

#[test]
fn retained_server_principals_keep_native_tagged_encoding() {
    use frona::core::Principal;
    for (principal, tag) in [
        (Principal::user("u"), "User"),
        (Principal::agent("a"), "Agent"),
        (Principal::mcp_server("m"), "McpServer"),
        (Principal::app("p"), "App"),
        (Principal::channel("c"), "Channel"),
    ] {
        let value = principal.clone().into_value();
        assert_eq!(
            value.clone().into_json_value(),
            json!({"kind": {tag: {}}, "id": principal.id})
        );
        assert_eq!(Principal::from_value(value).unwrap(), principal);
    }
}
