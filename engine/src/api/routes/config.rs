use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::core::config::{
    Config, config_file_path, deep_merge, redact_config_for_api,
};
use crate::core::state::AppState;

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;


pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/config/schema", get(get_schema))
        .route("/api/config", get(get_config).put(update_config))
}

async fn get_schema(_auth: AuthUser) -> Json<serde_json::Value> {
    let schema = schemars::schema_for!(Config);
    Json(serde_json::to_value(schema).unwrap_or_default())
}

async fn get_config(
    _auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut value = serde_json::to_value(state.config.as_ref())
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(e.to_string())))?;
    redact_config_for_api(&mut value);
    Ok(Json(value))
}

async fn update_config(
    _auth: AuthUser,
    State(state): State<AppState>,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let path = config_file_path();

    // Read raw YAML from disk (preserving ${VAR} references)
    let raw_yaml = std::fs::read_to_string(&path).unwrap_or_default();
    let mut base: serde_json::Value = if raw_yaml.is_empty() {
        serde_json::json!({})
    } else {
        // Parse YAML → serde_yaml::Value → serde_json::Value
        let yaml_val: serde_yaml::Value = serde_yaml::from_str(&raw_yaml)
            .map_err(|e| ApiError(crate::core::error::AppError::Internal(
                format!("Failed to parse existing config.yaml: {e}"),
            )))?;
        serde_json::to_value(yaml_val)
            .map_err(|e| ApiError(crate::core::error::AppError::Internal(
                format!("Failed to convert YAML to JSON: {e}"),
            )))?
    };

    // Deep-merge patch into base
    deep_merge(&mut base, patch);

    // Validate by deserializing through Config
    let _: Config = serde_json::from_value(base.clone())
        .map_err(|e| ApiError(crate::core::error::AppError::Validation(
            format!("Invalid config: {e}"),
        )))?;

    // Round-trip through JSON string → YAML value to avoid
    // $serde_json::private::Number serialization artifacts
    let json_str = serde_json::to_string(&base)
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(
            format!("Failed to serialize config: {e}"),
        )))?;
    let yaml_val: serde_yaml::Value = serde_yaml::from_str(&json_str)
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(
            format!("Failed to convert config to YAML: {e}"),
        )))?;
    let yaml_str = serde_yaml::to_string(&yaml_val)
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(
            format!("Failed to serialize config: {e}"),
        )))?;

    // Ensure parent directory exists
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Write to disk
    std::fs::write(&path, &yaml_str)
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(
            format!("Failed to write config file: {e}"),
        )))?;

    // Mark setup as completed
    state.set_runtime_config("setup_completed", "true").await?;

    // Return redacted config
    redact_config_for_api(&mut base);

    Ok(Json(serde_json::json!({
        "config": base,
        "restart_required": true,
    })))
}
