use axum::extract::{Path as AxumPath, Query, State};
use axum::response::Response;
use axum::Json;
use tokio::fs;

use crate::storage::{FileEntry, SearchTarget, VirtualPath, validate_relative_path};

use super::super::super::error::ApiError;
use super::super::super::middleware::auth::AuthUser;
use super::models::{FileAuth, SearchQuery};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub(crate) async fn download_user_file(
    file_auth: FileAuth,
    State(state): State<AppState>,
    AxumPath((username, filename)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    match file_auth {
        FileAuth::User(auth) => {
            if username != auth.username {
                return Err(ApiError(AppError::Forbidden(
                    "Cannot access another user's files".into(),
                )));
            }
        }
        FileAuth::Presigned { owner, path } => {
            if !owner.starts_with("user:") || path != filename {
                return Err(ApiError(AppError::Forbidden(
                    "Presigned URL does not match requested file".into(),
                )));
            }
        }
    }

    let vpath = VirtualPath::user(&username, &filename);
    super::serve_file(&vpath, &state).await
}

pub(crate) async fn download_agent_file(
    file_auth: FileAuth,
    State(state): State<AppState>,
    AxumPath((agent_id, filepath)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    match &file_auth {
        FileAuth::User(auth) => {
            state
                .agent_service
                .get(&auth.user_id, &agent_id)
                .await?;
        }
        FileAuth::Presigned { owner, path } => {
            if owner != &format!("agent:{agent_id}") || *path != filepath {
                return Err(ApiError(AppError::Forbidden(
                    "Presigned URL does not match requested file".into(),
                )));
            }
        }
    }

    let vpath = VirtualPath::agent(&agent_id, &filepath);
    super::serve_file(&vpath, &state).await
}

pub(crate) async fn delete_user_file(
    auth: AuthUser,
    State(state): State<AppState>,
    AxumPath((username, filename)): AxumPath<(String, String)>,
) -> Result<(), ApiError> {
    if username != auth.username {
        return Err(ApiError(AppError::Forbidden(
            "Cannot delete another user's files".into(),
        )));
    }

    let vpath = VirtualPath::user(&username, &filename);
    let resolved = state.storage_service.resolve(&vpath)?;

    if !resolved.exists() {
        return Err(ApiError(AppError::NotFound(
            "File not found".into(),
        )));
    }

    if resolved.is_dir() {
        fs::remove_dir_all(&resolved)
            .await
            .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
    } else {
        fs::remove_file(&resolved)
            .await
            .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
    }

    Ok(())
}

pub(crate) async fn list_user_files(
    auth: AuthUser,
    State(state): State<AppState>,
    dirpath: Option<AxumPath<String>>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    let rel = dirpath.map(|p| p.0).unwrap_or_default();
    if !rel.is_empty() {
        validate_relative_path(&rel)?;
    }

    let user_ws = state.storage_service.user_workspace(&auth.username);
    let base = user_ws.base_path().to_path_buf();
    let dir = if rel.is_empty() {
        base
    } else {
        base.join(&rel)
    };

    let parent_id = if rel.is_empty() {
        "/".to_string()
    } else {
        format!("/{rel}")
    };

    let entries = state.storage_service.list_dir(&dir, &parent_id).await?;
    Ok(Json(entries))
}

async fn list_agent_dir(
    auth: &AuthUser,
    state: &AppState,
    agent_id: &str,
    rel: &str,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    state.agent_service.get(&auth.user_id, agent_id).await?;

    if !rel.is_empty() {
        validate_relative_path(rel)?;
    }

    let ws = state.storage_service.agent_workspace(agent_id);
    let base = ws.base_path().to_path_buf();
    let dir = if rel.is_empty() {
        base
    } else {
        base.join(rel)
    };

    let parent_id = if rel.is_empty() {
        "/".to_string()
    } else {
        format!("/{rel}")
    };

    let entries = state.storage_service.list_dir(&dir, &parent_id).await?;
    Ok(Json(entries))
}

pub(crate) async fn list_agent_files_root(
    auth: AuthUser,
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    list_agent_dir(&auth, &state, &agent_id, "").await
}

pub(crate) async fn list_agent_files_subdir(
    auth: AuthUser,
    State(state): State<AppState>,
    AxumPath((agent_id, dirpath)): AxumPath<(String, String)>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    list_agent_dir(&auth, &state, &agent_id, &dirpath).await
}

pub(crate) async fn search_files(
    auth: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    if query.q.is_empty() {
        return Ok(Json(vec![]));
    }

    let mut targets: Vec<SearchTarget> = Vec::new();

    match query.scope.as_deref() {
        Some(scope) if scope.starts_with("agent:") => {
            let rest = &scope["agent:".len()..];
            let (agent_id, subpath) = match rest.find('/') {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            state.agent_service.get(&auth.user_id, agent_id).await?;
            let ws = state.storage_service.agent_workspace(agent_id);
            let root = ws.base_path().to_path_buf();
            let dir = match subpath {
                Some(sub) => root.join(sub),
                None => root.clone(),
            };
            targets.push(SearchTarget { dir, root, source: agent_id.to_string() });
        }
        Some(scope) if scope.starts_with("user") => {
            let subpath = scope.strip_prefix("user:").unwrap_or("");
            let ws = state.storage_service.user_workspace(&auth.username);
            let base = ws.base_path().to_path_buf();
            let dir = if subpath.is_empty() {
                base.clone()
            } else {
                base.join(subpath)
            };
            targets.push(SearchTarget { dir, root: base, source: "user".to_string() });
        }
        _ => {
            let user_ws = state.storage_service.user_workspace(&auth.username);
            let user_dir = user_ws.base_path().to_path_buf();
            targets.push(SearchTarget { dir: user_dir.clone(), root: user_dir, source: "user".to_string() });

            let user_agents = state.agent_service.list(&auth.user_id).await?;
            for agent in &user_agents {
                let ws = state.storage_service.agent_workspace(&agent.id);
                let agent_dir = ws.base_path().to_path_buf();
                if agent_dir.is_dir() {
                    targets.push(SearchTarget { dir: agent_dir.clone(), root: agent_dir, source: agent.id.clone() });
                }
            }
        }
    }

    let results = state.storage_service.search(targets, &query.q).await?;
    Ok(Json(results))
}
