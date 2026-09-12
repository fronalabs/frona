//! Managed-vault login does not require a model provider configuration.
use crate::{
    api::{error::ApiError, middleware::auth::AuthUser},
    core::{error::AppError, state::AppState},
    credential::managed::{
        Key, ManagedVault,
        login::service::{LoginAttempt, LoginTarget},
    },
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/vaults/{id}/logins", post(start))
        .route(
            "/api/vaults/{id}/logins/{attempt}",
            get(status).delete(cancel),
        )
        .route("/api/vaults/{id}/logins/{attempt}/complete", post(complete))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    provider: String,
    target: Target,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Create { name: String },
    Replace { credential_id: Uuid },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    code: String,
}

async fn vault(
    state: &AppState,
    auth: &AuthUser,
    connection: &str,
) -> Result<ManagedVault, ApiError> {
    let vault = state
        .vault_service
        .managed_login_vault(&auth.user_id, connection)
        .await?;
    let connection = state
        .vault_service
        .get_connection(&auth.user_id, connection)
        .await?;
    if connection.system_managed {
        auth.require_admin(state).await?;
    }
    Ok(vault)
}
async fn start(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(connection): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<LoginAttempt>, ApiError> {
    let vault = vault(&state, &auth, &connection).await?;
    let body: StartRequest = super::providers::decode(body)?;
    let (key, metadata, expected_id) = match body.target {
        Target::Create { name } => {
            let name = name.trim();
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                return Err(ApiError(AppError::Validation(
                    "invalid credential name".into(),
                )));
            }
            // This key identifies only the in-memory attempt, not the saved credential.
            let key = Key::new(format!("credential:{name}"), "credential");
            if vault
                .list()
                .await?
                .iter()
                .any(|entry| entry.metadata["name"].as_str() == Some(name))
            {
                return Err(ApiError(AppError::Conflict(
                    "credential already exists; use its ID to log in again".into(),
                )));
            }
            (key, json!({"name": name}), None)
        }
        Target::Replace { credential_id } => {
            let current = vault
                .status_by_id(credential_id)
                .await?
                .filter(|s| !s.deleted)
                .ok_or_else(|| ApiError(AppError::NotFound("managed credential".into())))?;
            (current.key, current.metadata, Some(credential_id))
        }
    };
    Ok(Json(
        state
            .vault_service
            .login_service()
            .start(
                &auth.user_id,
                &body.provider,
                LoginTarget {
                    vault,
                    key,
                    metadata,
                    expected_id,
                },
            )
            .await?,
    ))
}
async fn status(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((connection, attempt)): Path<(String, Uuid)>,
) -> Result<Json<LoginAttempt>, ApiError> {
    let vault = vault(&state, &auth, &connection).await?;
    state
        .vault_service
        .login_service()
        .check_vault(&auth.user_id, attempt, vault.connection_id())
        .await?;
    Ok(Json(
        state
            .vault_service
            .advance_login(&auth.user_id, attempt, None)
            .await?,
    ))
}
async fn cancel(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((connection, attempt)): Path<(String, Uuid)>,
) -> Result<Json<LoginAttempt>, ApiError> {
    let vault = vault(&state, &auth, &connection).await?;
    state
        .vault_service
        .login_service()
        .check_vault(&auth.user_id, attempt, vault.connection_id())
        .await?;
    Ok(Json(
        state
            .vault_service
            .login_service()
            .cancel(&auth.user_id, attempt)
            .await?,
    ))
}
async fn complete(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((connection, attempt)): Path<(String, Uuid)>,
    Json(body): Json<Value>,
) -> Result<Json<LoginAttempt>, ApiError> {
    let vault = vault(&state, &auth, &connection).await?;
    state
        .vault_service
        .login_service()
        .check_vault(&auth.user_id, attempt, vault.connection_id())
        .await?;
    let body: Completion = super::providers::decode(body)?;
    Ok(Json(
        state
            .vault_service
            .advance_login(&auth.user_id, attempt, Some(&body.code))
            .await?,
    ))
}
