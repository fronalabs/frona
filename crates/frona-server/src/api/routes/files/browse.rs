use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::response::Response;
use tokio::fs;

use crate::storage::path::{resolve_filesystem_path, resolve_workspace_path};

use crate::storage::{
    FileEntry, SearchTarget, VirtualPath, open_workspace_file, validate_relative_path,
};

use super::super::super::error::ApiError;
use super::super::super::middleware::auth::AuthUser;
use super::FileResponseQuery;
use super::models::{FileAuth, SearchQuery};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub(crate) async fn download_user_file(
    file_auth: FileAuth,
    State(state): State<AppState>,
    AxumPath((handle_raw, filename)): AxumPath<(String, String)>,
    Query(query): Query<FileResponseQuery>,
) -> Result<Response, ApiError> {
    let handle = crate::core::Handle::try_new(handle_raw)
        .map_err(|_| ApiError(AppError::NotFound("File not found".into())))?;
    match file_auth {
        FileAuth::User(auth) => {
            if handle != auth.handle {
                return Err(ApiError(AppError::Forbidden(
                    "Cannot access another user's files".into(),
                )));
            }
        }
        FileAuth::Presigned {
            sub: _,
            owner,
            path,
        } => {
            if !owner.starts_with("user:") || path != filename {
                return Err(ApiError(AppError::Forbidden(
                    "Presigned URL does not match requested file".into(),
                )));
            }
        }
    }

    let vpath = VirtualPath::user(&handle, &filename);
    super::serve_file(&vpath, &state, query.download).await
}

pub(crate) async fn download_agent_file(
    file_auth: FileAuth,
    State(state): State<AppState>,
    AxumPath((agent_seg, filepath)): AxumPath<(String, String)>,
    Query(query): Query<FileResponseQuery>,
) -> Result<Response, ApiError> {
    let (user_id, user_handle) = match &file_auth {
        FileAuth::User(auth) => (auth.user_id.clone(), auth.handle.clone()),
        FileAuth::Presigned { sub, owner, path } => {
            // Older avatar presigns used `agent:{uuid}`; accept both shapes.
            let expected_handle = format!("agent:{agent_seg}");
            let owner_ok = owner == &expected_handle || owner.starts_with("agent:");
            if !owner_ok || path != &filepath {
                return Err(ApiError(AppError::Forbidden(
                    "Presigned URL does not match requested file".into(),
                )));
            }
            let user_handle = state.user_service.handle_of(sub).await?;
            (sub.clone(), user_handle)
        }
    };

    let agent = state.agent_service.owned_by(&user_id, &agent_seg).await?;

    if let FileAuth::Presigned { owner, .. } = &file_auth
        && owner.starts_with("agent:")
        && owner != &format!("agent:{}", agent.handle)
        && owner != &format!("agent:{}", agent.id)
    {
        return Err(ApiError(AppError::Forbidden(
            "Presigned URL owner does not match requested agent".into(),
        )));
    }

    let workspace = state
        .storage_service
        .agent_workspace_path(&user_handle, &agent.handle);
    let relative = filepath.clone();
    let file = tokio::task::spawn_blocking(move || open_workspace_file(&workspace, &relative))
        .await
        .map_err(|error| AppError::Internal(format!("Failed to open download: {error}")))??;

    // Stream the already-opened file, preserving the requested symlink's display name.
    let filename = std::path::Path::new(&filepath)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    super::serve_open_file(fs::File::from_std(file), filename, query.download)
}

pub(crate) async fn delete_user_file(
    auth: AuthUser,
    State(state): State<AppState>,
    AxumPath((handle_raw, filename)): AxumPath<(String, String)>,
) -> Result<(), ApiError> {
    let handle = crate::core::Handle::try_new(handle_raw)
        .map_err(|_| ApiError(AppError::NotFound("File not found".into())))?;
    if handle != auth.handle {
        return Err(ApiError(AppError::Forbidden(
            "Cannot delete another user's files".into(),
        )));
    }

    let vpath = VirtualPath::user(&handle, &filename);
    let resolved = state.storage_service.resolve_virtual_path(&vpath)?;

    if !resolved.exists() {
        return Err(ApiError(AppError::NotFound("File not found".into())));
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

    let user_ws = state.storage_service.user_workspace(&auth.handle);
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
    agent_seg: &str,
    rel: &str,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    let agent = state
        .agent_service
        .owned_by(&auth.user_id, agent_seg)
        .await?;

    if !rel.is_empty() {
        validate_relative_path(rel)?;
    }

    let ws = state
        .storage_service
        .agent_workspace(&auth.handle, &agent.handle);
    let base = ws.base_path().to_path_buf();
    let dir = if rel.is_empty() { base } else { base.join(rel) };

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
            let (agent_seg, subpath) = match rest.find('/') {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            let agent = state
                .agent_service
                .owned_by(&auth.user_id, agent_seg)
                .await?;
            let ws = state
                .storage_service
                .agent_workspace(&auth.handle, &agent.handle);
            let root = resolve_filesystem_path(ws.base_path())?;
            let dir = match subpath {
                Some(sub) => resolve_workspace_path(&root, std::path::Path::new(sub))?,
                None => root.clone(),
            };
            targets.push(SearchTarget {
                dir,
                root,
                source: agent_seg.to_string(),
            });
        }
        Some(scope) if scope.starts_with("user") => {
            let subpath = scope.strip_prefix("user:").unwrap_or("");
            let ws = state.storage_service.user_workspace(&auth.handle);
            let base = resolve_filesystem_path(ws.base_path())?;
            let dir = if subpath.is_empty() {
                base.clone()
            } else {
                resolve_workspace_path(&base, std::path::Path::new(subpath))?
            };
            targets.push(SearchTarget {
                dir,
                root: base,
                source: "user".to_string(),
            });
        }
        _ => {
            let user_ws = state.storage_service.user_workspace(&auth.handle);
            let user_dir = resolve_filesystem_path(user_ws.base_path())?;
            targets.push(SearchTarget {
                dir: user_dir.clone(),
                root: user_dir,
                source: "user".to_string(),
            });

            let user_agents = state.agent_service.list(&auth.user_id).await?;
            for agent in &user_agents {
                let ws = state
                    .storage_service
                    .agent_workspace(&auth.handle, &agent.handle);
                let agent_dir = resolve_filesystem_path(ws.base_path())?;
                if agent_dir.is_dir() {
                    targets.push(SearchTarget {
                        dir: agent_dir.clone(),
                        root: agent_dir,
                        source: agent.id.clone(),
                    });
                }
            }
        }
    }

    let results = state.storage_service.search(targets, &query.q).await?;
    Ok(Json(results))
}
