use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::api::error::ApiError;
use crate::api::middleware::auth::AuthUser;
use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::credential::vault::models::*;
use crate::credential::vault::provider::create_vault_provider;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/vaults", post(create_connection).get(list_connections))
        .route("/api/vaults/grants", get(list_grants).post(create_grant))
        .route("/api/vaults/grants/{id}", delete(revoke_grant))
        .route(
            "/api/vaults/local/items",
            get(list_local_items).post(create_local_item),
        )
        .route(
            "/api/vaults/local/items/{id}",
            axum::routing::put(update_local_item).delete(delete_local_item),
        )
        .route("/api/vaults/test", post(test_vault))
        .route("/api/vaults/{id}", delete(delete_connection))
        .route("/api/vaults/{id}/toggle", post(toggle_connection))
        .route("/api/vaults/{id}/test", post(test_connection))
        .route(
            "/api/vaults/{id}/items",
            get(search_items).post(search_items_inline),
        )
        .route("/api/vaults/{id}/items/{item_id}/fields", get(item_fields))
        .route(
            "/api/vaults/{id}/items/{item_id}",
            delete(delete_managed_item),
        )
}

async fn delete_managed_item(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((connection_id, item_id)): Path<(String, uuid::Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let connection = state
        .vault_service
        .get_connection(&auth.user_id, &connection_id)
        .await?;
    if connection.system_managed {
        auth.require_admin(&state).await?;
    }
    state
        .vault_service
        .delete_managed_item(&auth.user_id, &connection_id, item_id)
        .await?;
    Ok(Json(serde_json::json!({"deleted": true})))
}

async fn create_connection(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateVaultConnectionRequest>,
) -> Result<Json<VaultConnectionResponse>, ApiError> {
    let response = state
        .vault_service
        .create_connection(&auth.user_id, req)
        .await?;
    Ok(Json(response))
}

async fn list_connections(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<VaultConnectionResponse>>, ApiError> {
    let connections = state.vault_service.list_connections(&auth.user_id).await?;
    Ok(Json(connections))
}

async fn delete_connection(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .vault_service
        .delete_connection(&auth.user_id, &id)
        .await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn toggle_connection(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ToggleVaultConnectionRequest>,
) -> Result<Json<VaultConnectionResponse>, ApiError> {
    let response = state
        .vault_service
        .toggle_connection(&auth.user_id, &id, req.enabled)
        .await?;
    Ok(Json(response))
}

async fn test_connection(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .vault_service
        .test_connection(&auth.user_id, &id)
        .await?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_max_results() -> usize {
    10
}

async fn search_items(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<VaultItem>>, ApiError> {
    let items = state
        .vault_service
        .search_items(&auth.user_id, &id, &query.q, query.max_results)
        .await?;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct InlineSearchRequest {
    provider: VaultProviderType,
    config: VaultConnectionConfig,
    #[serde(default)]
    q: String,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

async fn search_items_inline(
    _auth: AuthUser,
    Path(_id): Path<String>,
    Json(req): Json<InlineSearchRequest>,
) -> Result<Json<Vec<VaultItem>>, ApiError> {
    let tmp = tempfile::tempdir()
        .map_err(|e| ApiError::from(AppError::Tool(format!("Failed to create temp dir: {e}"))))?;
    let provider = create_vault_provider(req.provider, req.config, tmp.path().to_path_buf())?;
    let items = provider.search(&req.q, req.max_results).await?;
    Ok(Json(items))
}

async fn list_grants(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<VaultGrantResponse>>, ApiError> {
    let grants = state.vault_service.list_grants(&auth.user_id).await?;
    Ok(Json(grants))
}

async fn revoke_grant(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.vault_service.revoke_grant(&auth.user_id, &id).await?;
    Ok(Json(serde_json::json!({ "revoked": true })))
}

async fn create_grant(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateGrantRequest>,
) -> Result<Json<VaultGrantResponse>, ApiError> {
    let grant = state
        .vault_service
        .create_grant(
            &auth.user_id,
            req.principal.clone(),
            &req.connection_id,
            &req.vault_item_id,
            &req.query,
            &GrantDuration::Permanent,
        )
        .await?;

    state
        .vault_service
        .create_binding(
            &auth.user_id,
            req.principal,
            &req.query,
            &req.connection_id,
            &req.vault_item_id,
            req.target,
            BindingScope::Durable,
            None,
        )
        .await?;

    Ok(Json(grant.into()))
}

async fn item_fields(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((connection_id, item_id)): Path<(String, String)>,
) -> Result<Json<Vec<String>>, ApiError> {
    let secret = state
        .vault_service
        .get_secret(&auth.user_id, &connection_id, &item_id)
        .await?;
    let mut fields = Vec::new();
    if secret.username.is_some() {
        fields.push("USERNAME".to_string());
    }
    if secret.password.is_some() {
        fields.push("PASSWORD".to_string());
    }
    for key in secret.fields.keys() {
        fields.push(key.to_uppercase().replace(' ', "_"));
    }
    Ok(Json(fields))
}

// --- Inline test route ---

#[derive(Deserialize)]
struct TestVaultRequest {
    provider: VaultProviderType,
    config: VaultConnectionConfig,
}

async fn test_vault(
    _auth: AuthUser,
    Json(req): Json<TestVaultRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.provider == VaultProviderType::Managed {
        if !matches!(req.config, VaultConnectionConfig::Managed {}) {
            return Err(AppError::Validation("invalid managed vault config".into()).into());
        }
        return Ok(Json(serde_json::json!({"status": "ok"})));
    }
    let tmp = tempfile::tempdir()
        .map_err(|e| ApiError::from(AppError::Tool(format!("Failed to create temp dir: {e}"))))?;
    let provider = create_vault_provider(req.provider, req.config, tmp.path().to_path_buf())?;
    provider.test_connection().await?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// --- Local item routes ---

async fn create_local_item(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateLocalItemRequest>,
) -> Result<Json<CredentialResponse>, ApiError> {
    let response = state
        .vault_service
        .create_credential(&auth.user_id, req)
        .await?;
    Ok(Json(response))
}

async fn list_local_items(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<CredentialResponse>>, ApiError> {
    let credentials = state.vault_service.list_credentials(&auth.user_id).await?;
    Ok(Json(credentials))
}

async fn update_local_item(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateLocalItemRequest>,
) -> Result<Json<CredentialResponse>, ApiError> {
    let response = state
        .vault_service
        .update_credential(&auth.user_id, &id, req)
        .await?;
    Ok(Json(response))
}

async fn delete_local_item(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .vault_service
        .delete_credential(&auth.user_id, &id)
        .await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}
