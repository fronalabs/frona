use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use super::super::error::ApiError;
use super::super::middleware::auth::AdminUser;
use crate::core::config::SaveResult;
use crate::core::config::{Config, redact_config_for_api};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/config/schema", get(get_schema))
        .route(
            "/api/config/environment-variables",
            get(environment_variables),
        )
        .route("/api/config", get(get_config).put(update_config))
}

async fn environment_variables(_admin: AdminUser) -> Json<Vec<String>> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            let name = name.to_ascii_uppercase();
            ["API", "KEY", "TOKEN", "SECRET", "CREDENTIAL"]
                .iter()
                .any(|keyword| name.contains(keyword))
        })
        .collect();
    names.sort_unstable();
    Json(names)
}

async fn get_schema(
    auth: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.0.require_admin(&state).await?;
    Ok(Json(
        serde_json::to_value(schemars::schema_for!(Config)).unwrap_or_default(),
    ))
}

async fn get_config(
    auth: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.0.require_admin(&state).await?;
    response(state.config_service.persisted()?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveRequest {
    patch: Value,
    expected_persisted_revision: String,
}

async fn update_config(
    auth: AdminUser,
    State(state): State<AppState>,
    Json(request): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    auth.0.require_admin(&state).await?;
    let (patch, expected) = if request.get("patch").is_some() {
        let request: SaveRequest = serde_json::from_value(request)
            .map_err(|error| ApiError(AppError::Validation(error.to_string())))?;
        (request.patch, Some(request.expected_persisted_revision))
    } else {
        (request, None)
    };
    let result = state
        .config_service
        .save(patch, expected.as_deref())
        .await?;
    state.set_runtime_config("setup_completed", "true").await?;
    response(result)
}

pub(super) fn response(result: SaveResult) -> Result<Json<Value>, ApiError> {
    response_with_env(result, std::env::vars().collect())
}

fn response_with_env(
    mut result: SaveResult,
    env: std::collections::HashMap<String, String>,
) -> Result<Json<Value>, ApiError> {
    let parsed =
        crate::core::config::ConfigService::resolve_document_with_env(&result.config, env)?;
    let parameter_overrides = crate::inference::config::ModelRegistryConfig {
        providers: parsed.providers.clone(),
        models: parsed.models.clone(),
        skip_auto_discover: true,
    }
    .parameter_overrides()
    .map_err(|error| ApiError(AppError::Validation(error.to_string())))?;
    let mut config = serde_json::to_value(parsed)
        .map_err(|error| ApiError(AppError::Internal(error.to_string())))?;
    redact_config_for_api(&mut config);
    redact_config_for_api(&mut result.config);
    Ok(Json(serde_json::json!({
        "config":config,
        "authoring_document":result.config,
        "persisted_revision":result.persisted_revision,
        "active_revision":result.active_revision,
        "restart_required":result.restart_required,
        "parameter_overrides":parameter_overrides,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::User,
        core::{Handle, Principal, repository::Repository},
        db::repo::generic::SurrealRepo,
    };
    use std::sync::Arc;

    #[test]
    fn config_response_reflects_environment_overrides_without_changing_authoring_document() {
        for document in [
            serde_json::json!({}),
            serde_json::json!({
                "server": {"port": 4321, "backend_url": "http://old", "frontend_url": "http://old", "cors_origins": "http://old"},
                "browser": null,
                "search": {"searxng_base_url": "http://old"},
            }),
        ] {
            let result = SaveResult {
                config: document.clone(),
                persisted_revision: "persisted".into(),
                active_revision: "active".into(),
                restart_required: true,
            };
            let env = [
                ("FRONA_SERVER_CORS_ORIGINS", "https://app.example.com"),
                ("FRONA_SERVER_BACKEND_URL", "https://api.example.com"),
                ("FRONA_SERVER_FRONTEND_URL", "https://app.example.com"),
                ("FRONA_LOG_LEVEL", "debug"),
                ("FRONA_BROWSER_WS_URL", "ws://browserless:3333"),
                ("FRONA_SEARCH_SEARXNG_BASE_URL", "http://searxng:8080"),
                ("FRONA_SERVER_PORT", "3001"),
                ("FRONA_AUTH_ENCRYPTION_SECRET", "test-env-secret"),
                ("FRONA_SERVER_DATA_DIR", "/srv/frona"),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
            let response = response_with_env(result, env)
                .map_err(|error| error.0)
                .unwrap()
                .0;
            let config = &response["config"];
            assert_eq!(config["browser"]["ws_url"], "ws://browserless:3333");
            assert_eq!(config["search"]["searxng_base_url"], "http://searxng:8080");
            assert_eq!(config["server"]["cors_origins"], "https://app.example.com");
            assert_eq!(config["server"]["backend_url"], "https://api.example.com");
            assert_eq!(config["server"]["frontend_url"], "https://app.example.com");
            assert_eq!(config["server"]["port"], 3001);
            assert_eq!(config["database"]["path"], "/srv/frona/db");
            assert_eq!(config["storage"]["data_dir"], "/srv/frona");
            assert_eq!(
                config["auth"]["encryption_secret"],
                serde_json::json!({"is_set": true})
            );
            assert!(!response.to_string().contains("test-env-secret"));
            assert_eq!(response["authoring_document"], document);
            assert_eq!(response["persisted_revision"], "persisted");
            assert_eq!(response["active_revision"], "active");
            assert_eq!(response["restart_required"], true);
        }
    }

    #[test]
    fn config_response_preserves_pending_edits_and_literal_model_parameters() {
        let parameters = serde_json::json!({"a.b": "${LITERAL_PARAMETER}", "CamelCase": null});
        let document = serde_json::json!({
            "server": {"port": 4321},
            "providers": {"openai": {"api_key": "${CONFIG_TEST_PROVIDER_KEY}"}},
            "models": {"primary": {"provider": "openai", "model": "test", "extra_params": parameters}},
        });
        let result = SaveResult {
            config: document,
            persisted_revision: "persisted".into(),
            active_revision: "active".into(),
            restart_required: true,
        };
        let response = response_with_env(result, Default::default())
            .map_err(|error| error.0)
            .unwrap()
            .0;
        assert_eq!(response["config"]["server"]["port"], 4321);
        assert_eq!(
            response["config"]["models"]["primary"]["extra_params"],
            parameters
        );
        assert_eq!(
            response["authoring_document"]["models"]["primary"]["extra_params"],
            parameters
        );
        assert_eq!(
            response["config"]["providers"]["openai"]["api_key"],
            serde_json::json!({"is_set": true})
        );
        assert_eq!(response["restart_required"], true);
    }

    fn auth(user: &User) -> AdminUser {
        AdminUser(crate::api::middleware::auth::AuthUser {
            user_id: user.id.clone(),
            handle: user.handle.clone(),
            email: user.email.clone(),
            token_id: "test".into(),
            token_type: "access".into(),
            principal: Principal::user(&user.id),
            scopes: None,
            extensions: None,
        })
    }

    #[tokio::test]
    async fn configuration_access_requires_admin_and_envelope_metadata_is_not_persisted() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let config = Config::default();
        let storage = crate::storage::StorageService::new(&config);
        let mut state = AppState::new(
            db.clone(),
            {
                let mut loaded = crate::core::config::ConfigService::load(
                    tempfile::tempdir().unwrap().path().join("config.yaml"),
                )
                .unwrap();
                loaded.config = config.clone();
                crate::core::config::ConfigService::new(loaded).unwrap()
            },
            Some(crate::inference::config::ModelRegistryConfig::empty()),
            storage,
            crate::core::metrics::setup_metrics_recorder(),
            Arc::new(
                crate::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                    80.0, 80.0, 90.0, 90.0,
                ),
            ),
            crate::app_state_fixture::catalogs(&config),
        );
        let path = directory.path().join("config.yaml");
        state.config_service = crate::core::config::ConfigService::new(
            crate::core::config::ConfigService::load(&path).unwrap(),
        )
        .unwrap();
        let repository: SurrealRepo<User> = SurrealRepo::new(db);
        let ordinary = User {
            id: "ordinary".into(),
            handle: Handle::const_validated("ordinary"),
            email: "ordinary@test.invalid".into(),
            name: "ordinary".into(),
            password_hash: String::new(),
            timezone: None,
            groups: vec![],
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let admin = User {
            id: "admin".into(),
            handle: Handle::const_validated("admin"),
            email: "admin@test.invalid".into(),
            groups: vec![crate::auth::models::ADMINS_GROUP.into()],
            ..ordinary.clone()
        };
        repository.create(&ordinary).await.unwrap();
        repository.create(&admin).await.unwrap();
        assert!(matches!(
            get_config(auth(&ordinary), State(state.clone()))
                .await
                .unwrap_err()
                .0,
            AppError::Forbidden(_)
        ));
        assert!(matches!(
            get_schema(auth(&ordinary), State(state.clone()))
                .await
                .unwrap_err()
                .0,
            AppError::Forbidden(_)
        ));
        assert!(matches!(
            update_config(
                auth(&ordinary),
                State(state.clone()),
                Json(serde_json::json!({"server":{"port":4321}}))
            )
            .await
            .unwrap_err()
            .0,
            AppError::Forbidden(_)
        ));
        assert!(!path.exists());
        std::fs::write(
            &path,
            "providers:\n  openai:\n    api_key: legacy-provider-secret\n",
        )
        .unwrap();
        let before = get_config(auth(&admin), State(state.clone()))
            .await
            .map_err(|error| error.0)
            .unwrap()
            .0;
        let result = update_config(auth(&admin), State(state.clone()), Json(serde_json::json!({
            "patch":{"server":{"port":4321}}, "expected_persisted_revision":before["persisted_revision"]
        }))).await.map_err(|error| error.0).unwrap().0;
        assert_eq!(result["restart_required"], true);
        assert_eq!(result["active_revision"], before["active_revision"]);
        assert_ne!(result["persisted_revision"], before["persisted_revision"]);
        assert_eq!(state.config.server.port, 3001);
        let authoring = get_config(auth(&admin), State(state.clone()))
            .await
            .map_err(|error| error.0)
            .unwrap()
            .0;
        assert_eq!(authoring["authoring_document"]["server"]["port"], 4321);
        let reloaded = crate::core::config::ConfigService::load(&path).unwrap();
        assert_eq!(
            authoring["config"]["server"]["port"],
            reloaded.config.server.port
        );
        let raw = serde_json::json!({"temperature":null,"nested":{"value":null},"array":[null,{"is_set":true}],"is_set":true,"a.b":"literal","template":"${RAW_LITERAL_DO_NOT_EXPAND}"});
        let saved = update_config(auth(&admin), State(state.clone()), Json(serde_json::json!({
            "patch":{"providers":{"openai":{"api_key":{"is_set":true}}}, "models":{"primary":{"provider":"openai","model":"fixture","temperature":0.4,"extra_params":raw}}},
            "expected_persisted_revision":authoring["persisted_revision"]
        }))).await.map_err(|error| error.0).unwrap().0;
        assert_eq!(
            saved["parameter_overrides"],
            serde_json::json!([{"config_path":"models.primary.temperature","wire_path":["temperature"]}])
        );
        assert_eq!(
            saved["authoring_document"]["models"]["primary"]["extra_params"],
            raw
        );
        let reread = get_config(auth(&admin), State(state.clone()))
            .await
            .map_err(|error| error.0)
            .unwrap()
            .0;
        assert_eq!(reread["config"]["models"]["primary"]["extra_params"], raw);
        assert_eq!(
            reread["authoring_document"]["models"]["primary"]["extra_params"],
            raw
        );
        assert!(
            update_config(
                auth(&admin),
                State(state.clone()),
                Json(serde_json::json!({"models":{"primary":{"extra_params":null}}}))
            )
            .await
            .is_err()
        );
        let yaml = std::fs::read_to_string(&path).unwrap();
        let document: Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(document["models"]["primary"]["extra_params"], raw);
        assert_eq!(
            document["providers"]["openai"]["api_key"],
            "legacy-provider-secret"
        );
        assert!(!yaml.contains("validation_ids"));
        assert!(!yaml.contains("expected_persisted_revision"));
    }
}
