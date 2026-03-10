use frona::core::config::{deep_merge, redact_config_for_api, redact_config_for_log, config_file_path, Config};
use serde_json::json;

#[test]
fn test_config_file_path_default() {
    // Without env vars set, returns data/config.yaml
    let path = config_file_path();
    assert!(path.ends_with("config.yaml"));
}

#[test]
fn test_redact_config_for_log() {
    let config = Config::default();
    let mut value = serde_json::to_value(&config).unwrap();
    redact_config_for_log(&mut value);

    assert_eq!(
        value["auth"]["encryption_secret"],
        json!("[redacted]")
    );
}

#[test]
fn test_redact_config_for_api() {
    let config = Config::default();
    let mut value = serde_json::to_value(&config).unwrap();
    redact_config_for_api(&mut value);

    // encryption_secret is the default placeholder, so is_set should be false
    assert_eq!(
        value["auth"]["encryption_secret"],
        json!({"is_set": false})
    );

    // sso.client_secret is None by default, so is_set should be false
    assert_eq!(
        value["sso"]["client_secret"],
        json!({"is_set": false})
    );
}

#[test]
fn test_redact_config_for_api_providers() {
    let mut config = Config::default();
    config.providers.insert("anthropic".into(), frona::core::config::ModelProviderConfig {
        api_key: Some("sk-secret".into()),
        base_url: None,
        enabled: true,
    });

    let mut value = serde_json::to_value(&config).unwrap();
    redact_config_for_api(&mut value);

    assert_eq!(
        value["providers"]["anthropic"]["api_key"],
        json!({"is_set": true})
    );
    // enabled should not be redacted
    assert_eq!(value["providers"]["anthropic"]["enabled"], json!(true));
}

#[test]
fn test_deep_merge_objects() {
    let mut base = json!({"a": 1, "b": {"c": 2, "d": 3}});
    let patch = json!({"b": {"c": 42}});
    deep_merge(&mut base, patch);
    assert_eq!(base, json!({"a": 1, "b": {"c": 42, "d": 3}}));
}

#[test]
fn test_deep_merge_null_removes_key() {
    let mut base = json!({"a": 1, "b": 2});
    let patch = json!({"b": null});
    deep_merge(&mut base, patch);
    assert_eq!(base, json!({"a": 1}));
}

#[test]
fn test_deep_merge_skips_redaction_markers() {
    let mut base = json!({"auth": {"encryption_secret": "real-secret"}});
    let patch = json!({"auth": {"encryption_secret": {"is_set": true}}});
    deep_merge(&mut base, patch);
    assert_eq!(base["auth"]["encryption_secret"], json!("real-secret"));
}

#[test]
fn test_deep_merge_overwrites_non_objects() {
    let mut base = json!({"a": "old"});
    let patch = json!({"a": "new", "b": 42});
    deep_merge(&mut base, patch);
    assert_eq!(base, json!({"a": "new", "b": 42}));
}

#[test]
fn test_json_schema_generation() {
    let schema = schemars::schema_for!(Config);
    let value = serde_json::to_value(schema).unwrap();

    // Should have properties for top-level config sections
    let props = value["properties"].as_object().unwrap();
    assert!(props.contains_key("server"));
    assert!(props.contains_key("auth"));
    assert!(props.contains_key("providers"));
    assert!(props.contains_key("models"));

    // Server port should have a description
    let server_ref = &value["properties"]["server"];
    // Navigate through $ref to find server properties
    assert!(server_ref.is_object());
}

#[tokio::test]
async fn test_runtime_config_operations() {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .unwrap();
    frona::api::db::setup_schema(&db).await.unwrap();

    let metrics_handle = frona::core::metrics::setup_metrics_recorder();
    let config = Config::default();
    let workspaces = frona::agent::workspace::AgentWorkspaceManager::new(
        &config.storage.workspaces_path,
        std::path::PathBuf::from(&config.storage.shared_config_dir).join("agents"),
    );
    let state = frona::core::state::AppState::new(db, &config, None, workspaces, metrics_handle);

    // Initially not set
    let val = state.get_runtime_config("setup_completed").await.unwrap();
    assert!(val.is_none());
    assert!(!state.get_runtime_config_bool("setup_completed").await);

    // Set the flag
    state.set_runtime_config("setup_completed", "true").await.unwrap();
    assert!(state.get_runtime_config_bool("setup_completed").await);

    // Overwrite
    state.set_runtime_config("setup_completed", "false").await.unwrap();
    assert!(!state.get_runtime_config_bool("setup_completed").await);

    // Different key
    state.set_runtime_config("other_flag", "hello").await.unwrap();
    let val = state.get_runtime_config("other_flag").await.unwrap();
    assert_eq!(val, Some("hello".to_string()));
}
