use super::super::{error::ApiError, middleware::auth::AdminUser};
use crate::{
    core::{Handle, error::AppError, state::AppState},
    inference::{credential::store::CredentialMethod, provider::service::*},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/config/provider-catalog", get(provider_catalog))
        .route("/api/config/providers", get(list))
        .route("/api/config/provider-credentials", get(saved_credentials))
        .route(
            "/api/config/providers/{handle}",
            get(inspect_saved).put(edit).delete(delete_provider),
        )
        .route(
            "/api/config/providers/{handle}/inspect",
            post(inspect_draft),
        )
        .route("/api/config/providers/{handle}/validate", post(validate))
        .route(
            "/api/config/providers/{handle}/credentials",
            get(inspect_saved).post(accept),
        )
        .route(
            "/api/config/providers/{handle}/credentials/{method}",
            delete(logout),
        )
        .route(
            "/api/config/providers/{handle}/drafts/{validation_id}",
            delete(discard),
        )
        .route(
            "/api/config/providers/{handle}/login/start",
            post(start_login),
        )
        .route(
            "/api/config/providers/{handle}/login/{attempt}",
            get(login_status).delete(cancel_login),
        )
        .route(
            "/api/config/providers/{handle}/login/{attempt}/complete",
            post(complete_login),
        )
}

pub(super) fn service(state: &AppState) -> &ModelProviderService {
    #[cfg(test)]
    PROVIDER_WORK.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    &state.model_provider_service
}

#[cfg(test)]
static PROVIDER_WORK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| {
        ApiError(AppError::Validation(
            "invalid provider request; check the request fields and types".into(),
        ))
    })
}

async fn list(_admin: AdminUser, State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut providers = service(&state).list().await?;
    let snapshot = state.catalog_sources.models.current();
    for provider in &mut providers {
        crate::inference::directory::providers::decorate(provider, &snapshot);
    }
    Ok(Json(json!({"providers":providers})))
}
async fn provider_catalog(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Json<crate::inference::directory::providers::ProviderCatalog> {
    #[cfg(test)]
    PROVIDER_WORK.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Json(crate::inference::directory::providers::catalog(
        &state.catalog_sources,
        chrono::Utc::now(),
    ))
}
async fn inspect_saved(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
) -> Result<Json<ProviderInspection>, ApiError> {
    let mut provider = service(&state).inspect(&handle, None).await?;
    crate::inference::directory::providers::decorate(
        &mut provider,
        &state.catalog_sources.models.current(),
    );
    Ok(Json(provider))
}
async fn inspect_draft(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    Json(body): Json<Value>,
) -> Result<Json<ProviderInspection>, ApiError> {
    let request: InspectRequest = decode(body)?;
    let mut provider = service(&state)
        .inspect(&handle, Some(request.config))
        .await?;
    crate::inference::directory::providers::decorate(
        &mut provider,
        &state.catalog_sources.models.current(),
    );
    Ok(Json(provider))
}
async fn validate(
    admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    Json(body): Json<Value>,
) -> Result<Json<ValidationResult>, ApiError> {
    let result = service(&state)
        .validate(&admin.0.user_id, &handle, decode(body)?)
        .await?;
    Ok(Json(result))
}
async fn accept(
    admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    Json(body): Json<Value>,
) -> Result<Json<PublicCredential>, ApiError> {
    Ok(Json(
        service(&state)
            .accept(&admin.0.user_id, &handle, decode(body)?)
            .await?,
    ))
}

async fn saved_credentials(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<SavedCredential>>, ApiError> {
    Ok(Json(service(&state).saved_credentials().await?))
}

async fn edit(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    super::config::response(service(&state).edit(&handle, decode(body)?).await?)
}
async fn delete_provider(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    super::config::response(service(&state).delete(&handle, decode(body)?).await?)
}
async fn logout(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path((handle, method)): Path<(Handle, String)>,
    Json(body): Json<Value>,
) -> Result<Json<MutationResult>, ApiError> {
    let method: CredentialMethod = decode(Value::String(method))?;
    Ok(Json(
        service(&state)
            .logout(&handle, method, decode(body)?)
            .await?,
    ))
}
async fn discard(
    admin: AdminUser,
    State(state): State<AppState>,
    Path((handle, validation_id)): Path<(Handle, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"discarded":service(&state).discard(&admin.0.user_id, &handle, validation_id).await?}),
    ))
}
async fn start_login(
    admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let (provider, target) = service(&state)
        .prepare_login(&handle, decode(body)?)
        .await?;
    let attempt = state
        .vault_service
        .login_service()
        .start(&admin.0.user_id, provider, target)
        .await?;
    Ok(Json(
        serde_json::to_value(crate::inference::credential::setup::LoginAttempt::try_from(
            attempt,
        )?)
        .map_err(|_| ApiError(AppError::Internal("cannot encode login attempt".into())))?,
    ))
}

async fn login_status(
    admin: AdminUser,
    State(state): State<AppState>,
    Path((handle, attempt)): Path<(Handle, Uuid)>,
) -> Result<Json<crate::inference::credential::setup::LoginAttempt>, ApiError> {
    state
        .vault_service
        .login_service()
        .check_connection(&admin.0.user_id, attempt, handle.as_str())
        .await?;
    Ok(Json(
        state
            .vault_service
            .advance_login(&admin.0.user_id, attempt, None)
            .await?
            .try_into()?,
    ))
}
async fn cancel_login(
    admin: AdminUser,
    State(state): State<AppState>,
    Path((handle, attempt)): Path<(Handle, Uuid)>,
) -> Result<Json<crate::inference::credential::setup::LoginAttempt>, ApiError> {
    state
        .vault_service
        .login_service()
        .check_connection(&admin.0.user_id, attempt, handle.as_str())
        .await?;
    Ok(Json(
        state
            .vault_service
            .login_service()
            .cancel(&admin.0.user_id, attempt)
            .await?
            .try_into()?,
    ))
}
async fn complete_login(
    admin: AdminUser,
    State(state): State<AppState>,
    Path((handle, attempt)): Path<(Handle, Uuid)>,
    Json(body): Json<Value>,
) -> Result<Json<crate::inference::credential::setup::LoginAttempt>, ApiError> {
    let body: crate::inference::credential::setup::LoginCompletion = decode(body)?;
    state
        .vault_service
        .login_service()
        .check_connection(&admin.0.user_id, attempt, handle.as_str())
        .await?;
    Ok(Json(
        state
            .vault_service
            .advance_login(&admin.0.user_id, attempt, Some(&body.code))
            .await?
            .try_into()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{User, token::models::CreatePatRequest},
        core::{Principal, config::Config, repository::Repository},
        db::repo::generic::SurrealRepo,
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::{
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    #[tokio::test]
    async fn authorization_matrix_blocks_all_provider_work_and_keeps_secrets_out_of_payloads() {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&server)
            .await;
        for key in ["legacy-secret", "draft-secret"] {
            Mock::given(method("GET")).and(path("/models")).and(header("authorization", format!("Bearer {key}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list", "data":[{"id":"fixture", "object":"model", "created":0, "owned_by":"fixture"}]})))
                .with_priority(1).mount(&server).await;
        }
        let mut config = Config::default();
        config.storage.cache_dir = directory
            .path()
            .join("cache")
            .to_string_lossy()
            .into_owned();
        config.auth.encryption_secret = "test-encryption-secret".into();
        config.providers.insert(
            Handle::const_validated("account"),
            crate::core::config::ModelProviderConfig {
                provider: Some("openai".into()),
                base_url: Some(server.uri()),
                api_key: Some("legacy-secret".into()),
                ..Default::default()
            },
        );
        config.models.insert(
            "primary".into(),
            serde_json::from_value(json!({"provider":"account", "model":"fixture"})).unwrap(),
        );
        let yaml = directory.path().join("config.yaml");
        std::fs::write(&yaml, serde_yaml::to_string(&config).unwrap()).unwrap();
        let original = std::fs::read(&yaml).unwrap();
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
            Some(crate::inference::config::ModelRegistryConfig {
                providers: config.providers.clone(),
                models: config.models.clone(),
                skip_auto_discover: true,
            }),
            crate::storage::StorageService::new(&config),
            crate::core::metrics::setup_metrics_recorder(),
            Arc::new(
                crate::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                    80.0, 80.0, 90.0, 90.0,
                ),
            ),
            crate::app_state_fixture::catalogs(&config),
        );
        state.vault_service.sync_config_connections().await.unwrap();
        state.model_provider_service.validator = state
            .model_provider_service
            .validator
            .clone()
            .with_timeout(Duration::from_secs(1));
        let mut snapshot = frona_model_catalog::ModelCatalogSnapshot::empty();
        snapshot.entries.insert(
            "openai/fixture".into(),
            frona_model_catalog::catalog::ModelEntry {
                name: Some("Fixture name".into()),
                limit: frona_model_catalog::catalog::Limit {
                    context: 1234,
                    output: 123,
                    input: None,
                },
                ..Default::default()
            },
        );
        state.catalog_sources.models.swap(snapshot);
        state.config_service = crate::core::config::ConfigService::new(
            crate::core::config::ConfigService::load(&yaml).unwrap(),
        )
        .unwrap();
        state.model_provider_service.config_service = state.config_service.clone();
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
        let ordinary_token = state
            .token_service
            .create_session_pair(&state.keypair_service, &ordinary)
            .await
            .unwrap()
            .0;
        let admin_token = state
            .token_service
            .create_session_pair(&state.keypair_service, &admin)
            .await
            .unwrap()
            .0;
        let agent_token = state
            .token_service
            .create_pat(
                &state.keypair_service,
                &admin,
                CreatePatRequest {
                    name: "agent".into(),
                    expires_in_days: None,
                    scopes: None,
                    principal: Some(Principal::agent("agent")),
                },
            )
            .await
            .unwrap()
            .token;
        let app = router()
            .merge(super::super::config::router())
            .merge(super::super::provider_models::router())
            .with_state(state.clone());
        let revision = state.config_service.persisted().unwrap().persisted_revision;
        let draft = json!({"provider":"openai", "base_url":server.uri()});
        let nil = Uuid::nil().to_string();
        let cases = vec![
            ("GET", "/api/config/provider-catalog".to_string(), json!({})),
            (
                "GET",
                "/api/config/environment-variables".to_string(),
                json!({}),
            ),
            ("GET", "/api/config".to_string(), json!({})),
            ("GET", "/api/config/schema".to_string(), json!({})),
            (
                "PUT",
                "/api/config".to_string(),
                json!({"patch":{"server":{"port":4321}}, "expected_persisted_revision":revision}),
            ),
            ("GET", "/api/config/providers".to_string(), json!({})),
            (
                "GET",
                "/api/config/provider-credentials".to_string(),
                json!({}),
            ),
            (
                "GET",
                "/api/config/providers/account".to_string(),
                json!({}),
            ),
            (
                "PUT",
                "/api/config/providers/account".to_string(),
                json!({"config":draft, "expected_persisted_revision":revision}),
            ),
            (
                "DELETE",
                "/api/config/providers/account".to_string(),
                json!({"expected_persisted_revision":revision}),
            ),
            (
                "POST",
                "/api/config/providers/account/inspect".to_string(),
                json!({"config":draft}),
            ),
            (
                "POST",
                "/api/config/providers/account/validate".to_string(),
                json!({"config":draft, "credential":{"source":"api_key", "api_key":"draft-secret"}}),
            ),
            (
                "GET",
                "/api/config/providers/account/credentials".to_string(),
                json!({}),
            ),
            (
                "POST",
                "/api/config/providers/account/credentials".to_string(),
                json!({"config":draft,"validation_id":nil,"method":"api_key","source":"database"}),
            ),
            (
                "POST",
                "/api/config/providers/account/models".to_string(),
                json!({"config":draft,"manual_models":[]}),
            ),
            (
                "DELETE",
                "/api/config/providers/account/credentials/api_key".to_string(),
                json!({"expected_generation":0}),
            ),
            (
                "DELETE",
                format!("/api/config/providers/account/drafts/{nil}"),
                json!({}),
            ),
            (
                "POST",
                "/api/config/providers/account/login/start".to_string(),
                json!({"config":draft, "method":"oauth"}),
            ),
            (
                "GET",
                format!("/api/config/providers/account/login/{nil}"),
                json!({}),
            ),
            (
                "DELETE",
                format!("/api/config/providers/account/login/{nil}"),
                json!({}),
            ),
            (
                "POST",
                format!("/api/config/providers/account/login/{nil}/complete"),
                json!({"code":"redacted-fixture"}),
            ),
            (
                "GET",
                "/api/config/providers/account/models".to_string(),
                json!({}),
            ),
            (
                "POST",
                "/api/config/providers/account/models".to_string(),
                json!({"config":draft,"validation_id":nil,"method":"api_key","source":"database"}),
            ),
        ];
        for token in [
            None,
            Some(&ordinary_token),
            Some(&agent_token),
            Some(&admin_token),
        ] {
            let administrator = token == Some(&admin_token);
            for (method, path, body) in &cases {
                let before = PROVIDER_WORK.load(Ordering::SeqCst);
                let mut request = Request::builder()
                    .method(*method)
                    .uri(path)
                    .header("content-type", "application/json");
                if let Some(token) = token {
                    request = request.header("authorization", format!("Bearer {token}"));
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::from(body.to_string())).unwrap())
                    .await
                    .unwrap();
                let status = response.status().as_u16();
                let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                let payload = String::from_utf8(bytes.to_vec()).unwrap();
                if administrator {
                    assert!(
                        !matches!(status, 401 | 403 | 500),
                        "admin {method} {path}: {status}, {payload}"
                    );
                    if path == "/api/config/provider-catalog" {
                        let catalog: Value = serde_json::from_str(&payload).unwrap();
                        assert_eq!(status, 200);
                        assert_eq!(
                            catalog["source_status"]["models.dev"]["state"],
                            "unavailable"
                        );
                        assert!(
                            catalog["providers"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|entry| entry["id"] == "openai")
                        );
                    }
                    if path == "/api/config/environment-variables" {
                        assert_eq!(status, 200);
                        let names: Vec<String> = serde_json::from_str(&payload).unwrap();
                        let mut expected: Vec<String> = std::env::vars_os()
                            .filter_map(|(name, _)| name.into_string().ok())
                            .filter(|name| {
                                let name = name.to_ascii_uppercase();
                                ["API", "KEY", "TOKEN", "SECRET", "CREDENTIAL"]
                                    .iter()
                                    .any(|keyword| name.contains(keyword))
                            })
                            .collect();
                        expected.sort_unstable();
                        assert_eq!(names, expected);
                    }
                    if *method == "GET" && path == "/api/config/providers/account/models" {
                        let listing: Value = serde_json::from_str(&payload).unwrap();
                        assert_eq!(status, 200);
                        assert_eq!(listing["directory_status"], "live");
                        assert_eq!(listing["manual_entry"], true);
                        let row = listing["models"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|row| row["id"] == "fixture")
                            .unwrap();
                        assert_eq!(row["name"], "Fixture name");
                        assert_eq!(row["context_window"], 1234);
                        assert_eq!(row["max_tokens"], 123);
                    }
                } else {
                    assert_eq!(
                        status,
                        if token.is_none() { 401 } else { 403 },
                        "{method} {path}"
                    );
                    assert_eq!(PROVIDER_WORK.load(Ordering::SeqCst), before);
                    assert_eq!(std::fs::read(&yaml).unwrap(), original);
                    assert!(
                        state
                            .model_provider_service
                            .store
                            .vault()
                            .list()
                            .await
                            .unwrap()
                            .is_empty()
                    );
                    assert!(server.received_requests().await.unwrap().is_empty());
                }
                for secret in [
                    "legacy-secret",
                    "draft-secret",
                    "test-encryption-secret",
                    "\"ciphertext\":",
                    "\"nonce\":",
                    "\"refresh_token\":",
                    "\"access_token\":",
                ] {
                    assert!(
                        !payload.contains(secret),
                        "secret field leaked in {method} {path}"
                    );
                }
            }
        }
        assert!(PROVIDER_WORK.load(Ordering::SeqCst) > 0);
        assert!(!server.received_requests().await.unwrap().is_empty());
    }
}
