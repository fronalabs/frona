use super::*;
use std::path::PathBuf;

#[test]
fn test_expand_env_vars() {
    unsafe { std::env::set_var("TEST_KEY_123", "my-secret") };
    let result = expand_env_vars("key=${TEST_KEY_123}");
    assert_eq!(result, "key=my-secret");
    unsafe { std::env::remove_var("TEST_KEY_123") };
}

#[test]
fn test_expand_env_vars_missing() {
    let result = expand_env_vars("key=${NONEXISTENT_VAR_XYZ}");
    assert_eq!(result, "key=");
}

#[test]
fn defaults_are_sensible() {
    let config = Config::default();
    assert_eq!(config.server.port, 3001);
    assert_eq!(
        config.auth.encryption_secret,
        "dev-secret-change-in-production"
    );
    assert_eq!(config.database.path, "data/db");
    assert_eq!(config.storage.data_dir, "data");
    assert_eq!(config.storage.skills_dir, "data/skills");
    assert_eq!(config.memory.basic_space_compaction_secs, 3600);
    assert_eq!(config.memory.pkm_consolidation_max_tool_turns, 8);
    assert_eq!(config.memory.pkm_consolidation_max_submissions, 8);
    assert_eq!(config.memory.pkm_playbook_max_tool_turns, 20);
    assert_eq!(config.memory.pkm_playbook_max_submissions, 20);
    assert!(!config.sso.enabled);
    assert!(config.sso.signups_match_email);
    assert!(config.browser.is_none());
    assert!(config.server.cors_origins.is_none());
    assert!(config.server.base_url.is_none());
    assert_eq!(config.server.max_body_size_bytes, 104_857_600);
    assert!(config.search.provider.is_none());
    assert!(config.search.searxng_base_url.is_none());
    assert_eq!(config.inference.max_tool_turns, 200);
    assert_eq!(config.inference.default_max_tokens, 8192);
    assert_eq!(config.inference.compaction_trigger_pct, 80);
    assert_eq!(config.inference.history_truncation_pct, 90);
}

#[test]
fn provider_option_serialization_omits_none_values() {
    let cases = [
        (
            "OpenAICompatParams",
            serde_json::to_value(OpenAICompatParams {
                top_p: Some(0.8),
                ..Default::default()
            })
            .unwrap(),
            serde_json::json!({ "top_p": 0.8 }),
        ),
        (
            "GeminiThinkingConfig",
            serde_json::to_value(GeminiThinkingConfig {
                thinking_budget: 1024,
                include_thoughts: None,
            })
            .unwrap(),
            serde_json::json!({ "thinking_budget": 1024 }),
        ),
        (
            "AnthropicParams",
            serde_json::to_value(AnthropicParams {
                top_k: Some(40),
                ..Default::default()
            })
            .unwrap(),
            serde_json::json!({ "top_k": 40 }),
        ),
        (
            "OllamaParams",
            serde_json::to_value(OllamaParams {
                num_ctx: Some(8192),
                ..Default::default()
            })
            .unwrap(),
            serde_json::json!({ "num_ctx": 8192 }),
        ),
        (
            "GeminiParams",
            serde_json::to_value(GeminiParams {
                candidate_count: Some(1),
                ..Default::default()
            })
            .unwrap(),
            serde_json::json!({ "candidate_count": 1 }),
        ),
        (
            "ProviderModel::OpenAI",
            serde_json::to_value(ProviderModel::OpenAI {
                api: None,
                params: OpenAICompatParams::default(),
            })
            .unwrap(),
            serde_json::json!({ "provider": "openai" }),
        ),
    ];

    for (name, actual, expected) in cases {
        assert_eq!(actual, expected, "{name}");
    }
}

#[test]
fn env_var_overrides_multi_word_field() {
    // The key remapping (replace first _ with __) means FRONA_BROWSER_WS_URL
    // becomes browser__ws_url, which separator("__") resolves to browser.ws_url.
    let loaded = load_with_env_override("FRONA_BROWSER_WS_URL", "ws://custom:9999");
    assert_eq!(
        loaded.config.browser.as_ref().unwrap().ws_url,
        "ws://custom:9999"
    );
}

fn load_with_env_override(key: &str, value: &str) -> LoadedConfig {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.yaml");
    std::fs::write(&path, "server:\n  port: 4321\n").unwrap();
    ConfigService::load_with_env(&path, [(key.into(), value.into())].into()).unwrap()
}

#[test]
fn env_var_overrides_server_port() {
    let loaded = load_with_env_override("FRONA_SERVER_PORT", "9999");
    assert_eq!(loaded.config.server.port, 9999);
}

#[test]
fn env_var_overrides_database_path() {
    let loaded = load_with_env_override("FRONA_DATABASE_PATH", "/tmp/testdb");
    assert_eq!(loaded.config.database.path, "/tmp/testdb");
}

#[test]
fn env_var_overrides_sso_enabled() {
    let loaded = load_with_env_override("FRONA_SSO_ENABLED", "true");
    assert!(loaded.config.sso.enabled);
}

#[test]
fn env_var_overrides_auth_allow_registration() {
    let loaded = load_with_env_override("FRONA_AUTH_ALLOW_REGISTRATION", "false");
    assert!(!loaded.config.auth.allow_registration);
}

#[test]
fn server_timezone_explicit_valid_passes() {
    let mut server = ServerConfig {
        timezone: "Asia/Tokyo".to_string(),
        ..Default::default()
    };
    resolve_server_timezone(&mut server);
    assert_eq!(server.timezone, "Asia/Tokyo");
}

#[test]
#[should_panic(expected = "Invalid server.timezone")]
fn server_timezone_explicit_invalid_panics() {
    let mut server = ServerConfig {
        timezone: "Mars/Olympus".to_string(),
        ..Default::default()
    };
    resolve_server_timezone(&mut server);
}

#[test]
fn server_timezone_empty_falls_back_to_detection() {
    let mut server = ServerConfig::default();
    assert!(server.timezone.is_empty());
    resolve_server_timezone(&mut server);
    assert!(!server.timezone.is_empty());
    assert!(
        server.timezone.parse::<chrono_tz::Tz>().is_ok(),
        "detected timezone '{}' must be a valid IANA name",
        server.timezone
    );
}

#[test]
fn auth_allow_registration_defaults_to_true() {
    let config = AuthConfig::default();
    assert!(config.allow_registration);
}

#[test]
fn browser_config_http_base_url() {
    let config = BrowserConfig {
        ws_url: "ws://localhost:3333".into(),
        ..Default::default()
    };
    assert_eq!(config.http_base_url(), "http://localhost:3333");
}

#[test]
fn browser_config_profile_path() {
    let config = BrowserConfig {
        profiles_path: "/data/profiles".into(),
        ..Default::default()
    };
    let path = config.profile_path(&crate::handle!("bob"), "github");
    assert_eq!(path, PathBuf::from("/data/profiles/bob/github"));
}

#[test]
fn mcp_cache_path_defaults_to_none() {
    let mcp = McpConfig::default();
    assert!(mcp.cache_path.is_none());
}

#[test]
fn strip_defaults_removes_all_defaults() {
    let mut value = serde_json::to_value(Config::default()).unwrap();
    strip_defaults(&mut value);
    assert_eq!(value, serde_json::json!({}));
}

#[test]
fn strip_defaults_keeps_changed_values() {
    let mut value = serde_json::json!({
        "server": { "port": 8080, "static_dir": "/app/static" },
        "auth": { "encryption_secret": "dev-secret-change-in-production" },
    });
    strip_defaults(&mut value);
    assert_eq!(
        value,
        serde_json::json!({
            "server": { "port": 8080 },
        })
    );
}

#[test]
fn strip_defaults_keeps_non_default_fields() {
    let mut value = serde_json::json!({
        "server": { "cors_origins": "https://example.com" },
    });
    strip_defaults(&mut value);
    assert_eq!(
        value,
        serde_json::json!({
            "server": { "cors_origins": "https://example.com" },
        })
    );
}

#[test]
fn strip_defaults_handles_integer_vs_float() {
    let mut value = serde_json::json!({
        "sandbox": { "max_cpu_pct": 95, "max_memory_pct": 80 },
    });
    strip_defaults(&mut value);
    assert_eq!(value, serde_json::json!({}));
}

#[test]
fn strip_defaults_removes_provider_entry_defaults() {
    let mut value = serde_json::json!({
        "providers": {
            "anthropic": { "base_url": null, "enabled": true },
            "openai": { "api_key": "sk-123", "enabled": true },
        },
    });
    strip_defaults(&mut value);
    assert_eq!(
        value,
        serde_json::json!({
            "providers": {
                "anthropic": {},
                "openai": { "api_key": "sk-123" },
            },
        })
    );
}

#[test]
fn strip_defaults_keeps_provider_connections_when_all_fields_are_default() {
    let mut value = serde_json::json!({
        "providers": {
            "anthropic": { "base_url": null, "enabled": true },
        },
    });
    strip_defaults(&mut value);
    assert_eq!(value, serde_json::json!({"providers": {"anthropic": {}}}));
}

#[test]
fn strip_defaults_removes_model_group_entry_defaults() {
    let mut value = serde_json::json!({
        "models": {
            "coding": {
                "main": "anthropic/claude-opus-4-6",
                "fallbacks": [],
                "max_tokens": 32000,
                "temperature": null,
                "context_window": 200000,
                "retry": {
                    "max_retries": 10,
                    "initial_backoff_ms": 1000,
                    "backoff_multiplier": 2,
                    "max_backoff_ms": 60000,
                },
            },
        },
    });
    strip_defaults(&mut value);
    assert_eq!(
        value,
        serde_json::json!({
            "models": {
                "coding": {
                    "main": "anthropic/claude-opus-4-6",
                    "max_tokens": 32000,
                    "context_window": 200000,
                },
            },
        })
    );
}
