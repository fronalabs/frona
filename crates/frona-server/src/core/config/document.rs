use super::*;

/// Paths to sensitive config fields. Used for both log redaction and API response masking.
pub const SENSITIVE_PATHS: &[&[&str]] = &[
    &["auth", "encryption_secret"],
    &["sso", "client_secret"],
    &["voice", "twilio_account_sid"],
    &["voice", "twilio_auth_token"],
    &["vault", "onepassword_service_account_token"],
    &["vault", "bitwarden_client_secret"],
    &["vault", "bitwarden_master_password"],
    &["vault", "hashicorp_token"],
    &["vault", "keepass_password"],
];

pub(super) fn revision(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub(super) fn read_file(path: &std::path::Path) -> Result<Vec<u8>, crate::core::error::AppError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(crate::core::error::AppError::Internal(format!(
            "cannot read configuration: {error}"
        ))),
    }
}

/// Provider fields that are sensitive (applied to each provider in the map).
pub const SENSITIVE_PROVIDER_FIELDS: &[&str] = &["api_key"];

/// Panics on explicit invalid config (fail-fast at startup, not silently mis-schedule).
pub fn resolve_server_timezone(server: &mut ServerConfig) {
    if !server.timezone.is_empty() {
        if server.timezone.parse::<chrono_tz::Tz>().is_err() {
            panic!(
                "Invalid server.timezone '{}' - must be an IANA timezone (e.g. 'America/Los_Angeles', 'Asia/Tokyo', 'UTC')",
                server.timezone
            );
        }
        tracing::info!(timezone = %server.timezone, "Server timezone resolved (explicit)");
        return;
    }

    // iana_time_zone ignores TZ when /etc/timezone disagrees -> "Etc/UTC" in containers.
    if let Ok(tz_env) = std::env::var("TZ")
        && !tz_env.is_empty()
        && tz_env.parse::<chrono_tz::Tz>().is_ok()
    {
        tracing::info!(timezone = %tz_env, "Server timezone resolved (TZ env var)");
        server.timezone = tz_env;
        return;
    }

    let detected = iana_time_zone::get_timezone().ok();
    let resolved = detected
        .filter(|tz| tz.parse::<chrono_tz::Tz>().is_ok())
        .unwrap_or_else(|| "UTC".to_string());
    tracing::info!(timezone = %resolved, "Server timezone resolved (auto-detected)");
    server.timezone = resolved;
}

pub fn config_file_path() -> String {
    let data_dir = std::env::var("FRONA_SERVER_DATA_DIR").unwrap_or_else(|_| "data".into());
    std::env::var("FRONA_CONFIG").unwrap_or_else(|_| format!("{data_dir}/config.yaml"))
}

/// Redact sensitive fields in a config JSON value for logging (replaces with "[redacted]").
pub fn redact_config_for_log(value: &mut serde_json::Value) {
    for path in SENSITIVE_PATHS {
        redact(value, path);
    }
    if let Some(providers) = value.get_mut("providers").and_then(|p| p.as_object_mut()) {
        for provider in providers.values_mut() {
            for field in SENSITIVE_PROVIDER_FIELDS {
                redact(provider, &[field]);
            }
        }
    }
}

const DEFAULT_ENCRYPTION_SECRET: &str = "dev-secret-change-in-production";

/// Redact sensitive fields for API responses: replaces with `{"is_set": true/false}`.
pub fn redact_config_for_api(value: &mut serde_json::Value) {
    let has_default_secret = value
        .pointer("/auth/encryption_secret")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == DEFAULT_ENCRYPTION_SECRET);

    for path in SENSITIVE_PATHS {
        redact_as_is_set(value, path);
    }
    if let Some(providers) = value.get_mut("providers").and_then(|p| p.as_object_mut()) {
        for provider in providers.values_mut() {
            for field in SENSITIVE_PROVIDER_FIELDS {
                redact_as_is_set(provider, &[field]);
            }
        }
    }

    if has_default_secret && let Some(auth) = value.get_mut("auth").and_then(|a| a.as_object_mut())
    {
        auth.insert(
            "encryption_secret".into(),
            serde_json::json!({ "is_set": false }),
        );
    }
}

fn redact_as_is_set(value: &mut serde_json::Value, path: &[&str]) {
    match path {
        [] => {}
        [key] => {
            if let Some(v) = value.get_mut(*key) {
                let is_set = match v {
                    serde_json::Value::Null => false,
                    serde_json::Value::String(s) => !s.is_empty(),
                    _ => true,
                };
                *v = serde_json::json!({ "is_set": is_set });
            }
        }
        [key, rest @ ..] => {
            if let Some(child) = value.get_mut(*key) {
                redact_as_is_set(child, rest);
            }
        }
    }
}

fn redact(value: &mut serde_json::Value, path: &[&str]) {
    match path {
        [] => {}
        [key] => {
            if let Some(v) = value.get_mut(*key)
                && !v.is_null()
            {
                *v = serde_json::Value::String("[redacted]".into());
            }
        }
        [key, rest @ ..] => {
            if let Some(child) = value.get_mut(*key) {
                redact(child, rest);
            }
        }
    }
}

/// Recursively remove fields that match the default `Config` values,
/// keeping config.yaml minimal with only user-changed values.
pub fn strip_defaults(value: &mut serde_json::Value) {
    let defaults = serde_json::to_value(Config::default()).unwrap_or_default();
    strip_defaults_recursive(value, &defaults);

    strip_map_entry_defaults::<ModelProviderConfig>(value, "providers");
    strip_map_entry_defaults::<ModelGroupConfig>(value, "models");
}

fn strip_map_entry_defaults<T: Default + serde::Serialize>(
    value: &mut serde_json::Value,
    key: &str,
) {
    let Some(map) = value.get_mut(key).and_then(|v| v.as_object_mut()) else {
        return;
    };
    let entry_defaults = serde_json::to_value(T::default()).unwrap_or_default();
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        if let Some(entry) = map.get_mut(&k) {
            strip_defaults_recursive(entry, &entry_defaults);
            if entry.as_object().is_some_and(|o| o.is_empty()) {
                map.remove(&k);
            }
        }
    }
    if map.is_empty() {
        value.as_object_mut().unwrap().remove(key);
    }
}

fn strip_defaults_recursive(value: &mut serde_json::Value, defaults: &serde_json::Value) {
    let (Some(obj), Some(def_obj)) = (value.as_object_mut(), defaults.as_object()) else {
        return;
    };

    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        let Some(def_val) = def_obj.get(&key) else {
            continue;
        };
        let Some(val) = obj.get_mut(&key) else {
            continue;
        };

        if val.is_object() && def_val.is_object() {
            strip_defaults_recursive(val, def_val);
            if val.as_object().is_some_and(|o| o.is_empty()) {
                obj.remove(&key);
            }
        } else if values_equal(val, def_val) {
            obj.remove(&key);
        }
    }
}

fn values_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => a == b,
    }
}

/// Merge a configuration patch. Arrays replace in full. Redaction markers are
/// special only at credential paths; model extra_params objects replace in full.
pub fn deep_merge(base: &mut serde_json::Value, patch: serde_json::Value) {
    fn merge(base: &mut serde_json::Value, patch: serde_json::Value, path: &mut Vec<String>) {
        if is_extra_params_path(path) {
            *base = patch;
            return;
        }
        match patch {
            serde_json::Value::Object(values) => {
                if !base.is_object() {
                    *base = serde_json::json!({});
                }
                let base = base.as_object_mut().expect("object");
                for (key, value) in values {
                    path.push(key.clone());
                    let marker = value.as_object().is_some_and(|object| {
                        object.len() == 1
                            && object
                                .get("is_set")
                                .is_some_and(serde_json::Value::is_boolean)
                    });
                    if marker && is_sensitive_config_path(path) {
                        // Retain the existing secret, never persist the marker.
                    } else if value.is_null() {
                        base.remove(&key);
                    } else {
                        merge(
                            base.entry(key).or_insert(serde_json::Value::Null),
                            value,
                            path,
                        );
                    }
                    path.pop();
                }
            }
            value => *base = value,
        }
    }
    merge(base, patch, &mut Vec::new());
}

fn is_sensitive_config_path(path: &[String]) -> bool {
    SENSITIVE_PATHS.iter().any(|candidate| {
        candidate.len() == path.len() && candidate.iter().zip(path).all(|(a, b)| *a == b)
    }) || (path.len() == 3
        && path[0] == "providers"
        && SENSITIVE_PROVIDER_FIELDS.contains(&path[2].as_str()))
}

fn is_extra_params_path(path: &[String]) -> bool {
    path.len() >= 3
        && path[0] == "models"
        && path.last().is_some_and(|key| key == "extra_params")
        && path[2..path.len() - 1].chunks(2).all(|pair| {
            pair.len() == 2 && pair[0] == "fallbacks" && pair[1].parse::<usize>().is_ok()
        })
}

/// Validate replacement semantics before mutating the authoring document.
/// Fallback arrays are complete replacements, not patches by index or model ID.
pub fn merge_config_patch(
    base: &mut serde_json::Value,
    patch: serde_json::Value,
) -> Result<(), crate::core::error::AppError> {
    fn validate_model(
        model: &serde_json::Value,
        path: &str,
        complete: bool,
    ) -> Result<(), crate::core::error::AppError> {
        use crate::core::error::AppError;
        if complete
            && (!model
                .get("provider")
                .is_some_and(serde_json::Value::is_string)
                || !model.get("model").is_some_and(serde_json::Value::is_string))
        {
            return Err(AppError::Validation(format!(
                "{path}: fallback array replacements require complete provider and model fields"
            )));
        }
        if model
            .get("extra_params")
            .is_some_and(|params| !params.is_object())
        {
            return Err(AppError::Validation(format!(
                "{path}.extra_params: expected an object; use {{}} to clear it"
            )));
        }
        if let Some(fallbacks) = model.get("fallbacks").and_then(serde_json::Value::as_array) {
            for (index, fallback) in fallbacks.iter().enumerate() {
                validate_model(fallback, &format!("{path}.fallbacks.{index}"), true)?;
            }
        }
        Ok(())
    }
    if let Some(models) = patch.get("models").and_then(serde_json::Value::as_object) {
        for (name, model) in models {
            validate_model(model, &format!("models.{name}"), false)?;
        }
    }
    deep_merge(base, patch);
    Ok(())
}

/// Provider request JSON is literal data, including strings that resemble
/// environment references. Expand the surrounding configuration only.
pub fn expand_config_env_vars(input: &str) -> Result<String, crate::core::error::AppError> {
    use crate::core::error::AppError;
    use serde_json::Value;

    fn extract(model: &mut Value, pointer: String, fields: &mut Vec<(String, Value)>) {
        if let Some(value) = model
            .as_object_mut()
            .and_then(|model| model.remove("extra_params"))
        {
            fields.push((pointer.clone(), value));
        }
        if let Some(fallbacks) = model.get_mut("fallbacks").and_then(Value::as_array_mut) {
            for (index, fallback) in fallbacks.iter_mut().enumerate() {
                extract(fallback, format!("{pointer}/fallbacks/{index}"), fields);
            }
        }
    }
    let error = |error: serde_yaml::Error| AppError::Validation(error.to_string());
    let mut source: Value = serde_yaml::from_str(input).map_err(error)?;
    let mut fields = Vec::new();
    if let Some(models) = source.get_mut("models").and_then(Value::as_object_mut) {
        for (name, model) in models {
            let escaped = name.replace('~', "~0").replace('/', "~1");
            extract(model, format!("/models/{escaped}"), &mut fields);
        }
    }
    if fields.is_empty() {
        return Ok(expand_env_vars(input));
    }
    let expanded = expand_env_vars(&serde_yaml::to_string(&source).map_err(error)?);
    let mut expanded: Value = serde_yaml::from_str(&expanded).map_err(error)?;
    for (pointer, value) in fields {
        let model = expanded
            .pointer_mut(&pointer)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                AppError::Validation(
                    "environment expansion changed a model's identity while restoring extra_params"
                        .into(),
                )
            })?;
        model.insert("extra_params".into(), value);
    }
    serde_yaml::to_string(&expanded).map_err(error)
}

pub fn expand_env_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut var_name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var_name.push(c);
            }
            if let Ok(val) = std::env::var(&var_name) {
                result.push_str(&val);
            }
        } else {
            result.push(c);
        }
    }

    result
}
