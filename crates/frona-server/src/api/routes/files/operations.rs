use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use tokio::fs;

use crate::storage::{Namespace, VirtualPath, dedup_filename, validate_relative_path};

use super::super::super::error::ApiError;
use super::super::super::middleware::auth::AuthUser;
use super::models::{CopyMoveRequest, DeleteRequest, MkdirRequest, RenameRequest};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub(crate) async fn rename_user_file(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<RenameRequest>,
) -> Result<(), ApiError> {
    reject_workspace_root(&req.path, "rename")?;
    let resolved = resolve_file_virtual_path(&req.path, &auth.user_id, &state).await?;

    if !resolved.exists() {
        return Err(ApiError(AppError::NotFound("File not found".into())));
    }

    if req.new_name.contains('/') || req.new_name.contains("..") || req.new_name.contains('\0') {
        return Err(ApiError(AppError::Validation("Invalid filename".into())));
    }

    let dest = resolved
        .parent()
        .ok_or_else(|| ApiError(AppError::Internal("No parent dir".into())))?
        .join(&req.new_name);

    if dest.exists() {
        return Err(ApiError(AppError::Validation(
            "A file with that name already exists".into(),
        )));
    }

    fs::rename(&resolved, &dest)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    Ok(())
}

fn reject_workspace_root(path: &str, operation: &str) -> Result<(), ApiError> {
    let relative = if path.starts_with("user://") || path.starts_with("agent://") {
        VirtualPath::parse(path)?.relative
    } else {
        path.trim_start_matches('/').to_string()
    };
    if relative.is_empty() {
        return Err(ApiError(AppError::Validation(format!(
            "Cannot {operation} a workspace root"
        ))));
    }
    Ok(())
}

async fn resolve_file_virtual_path(
    path: &str,
    user_id: &str,
    state: &AppState,
) -> Result<PathBuf, ApiError> {
    let user_handle = state.user_service.handle_of(user_id).await?;
    let vpath = if path.starts_with("user://") || path.starts_with("agent://") {
        VirtualPath::parse(path)?
    } else {
        VirtualPath::user(&user_handle, path.trim_start_matches('/'))
    };

    match &vpath.namespace {
        Namespace::User(path_handle) if path_handle != user_handle.as_ref() => {
            return Err(ApiError(AppError::Forbidden(
                "Cannot access another user's files".into(),
            )));
        }
        Namespace::Agent(agent_handle) => {
            state.agent_service.owned_by(user_id, agent_handle).await?;
        }
        Namespace::User(_) => {}
    }

    state
        .storage_service
        .resolve_virtual_path_for_user(&user_handle, &vpath)
        .map_err(ApiError)
}

pub(crate) async fn copy_files(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CopyMoveRequest>,
) -> Result<(), ApiError> {
    if req.sources.is_empty() {
        return Err(ApiError(AppError::Validation(
            "At least one source is required".into(),
        )));
    }
    let dest_dir = resolve_file_virtual_path(&req.destination, &auth.user_id, &state).await?;
    let mut resolved_sources = Vec::with_capacity(req.sources.len());
    for source in &req.sources {
        reject_workspace_root(source, "copy")?;
        let src = resolve_file_virtual_path(source, &auth.user_id, &state).await?;
        if !src.exists() {
            return Err(ApiError(AppError::NotFound(format!(
                "Source file not found: {source}"
            ))));
        }
        if src.is_dir() && dest_dir.starts_with(&src) {
            return Err(ApiError(AppError::Validation(
                "Cannot copy a folder into itself".into(),
            )));
        }
        resolved_sources.push(src);
    }

    fs::create_dir_all(&dest_dir)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    for src in resolved_sources {
        let name = src
            .file_name()
            .ok_or_else(|| ApiError(AppError::Internal("No filename".into())))?
            .to_string_lossy()
            .into_owned();
        let target = dest_dir.join(dedup_filename(&dest_dir, &name));
        if src.is_dir() {
            copy_dir_recursive(&src, &target).await?;
        } else {
            fs::copy(&src, &target)
                .await
                .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
        }
    }

    Ok(())
}

async fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), ApiError> {
    fs::create_dir_all(dest)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    let mut read_dir = fs::read_dir(src)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?
    {
        let target = dest.join(entry.file_name());
        if entry
            .metadata()
            .await
            .map_err(|e| ApiError(AppError::Internal(e.to_string())))?
            .is_dir()
        {
            Box::pin(copy_dir_recursive(&entry.path(), &target)).await?;
        } else {
            fs::copy(entry.path(), &target)
                .await
                .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
        }
    }

    Ok(())
}

pub(crate) async fn move_files(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CopyMoveRequest>,
) -> Result<(), ApiError> {
    if req.sources.is_empty() {
        return Err(ApiError(AppError::Validation(
            "At least one source is required".into(),
        )));
    }
    let dest_dir = resolve_file_virtual_path(&req.destination, &auth.user_id, &state).await?;
    let mut planned_moves = Vec::with_capacity(req.sources.len());
    let mut targets = HashSet::with_capacity(req.sources.len());
    for source in &req.sources {
        reject_workspace_root(source, "move")?;
        let src = resolve_file_virtual_path(source, &auth.user_id, &state).await?;
        if !src.exists() {
            return Err(ApiError(AppError::NotFound(format!(
                "Source file not found: {source}"
            ))));
        }
        if src.is_dir() && dest_dir.starts_with(&src) {
            return Err(ApiError(AppError::Validation(
                "Cannot move a folder into itself".into(),
            )));
        }
        let name = src
            .file_name()
            .ok_or_else(|| ApiError(AppError::Internal("No filename".into())))?
            .to_string_lossy()
            .into_owned();
        let target = dest_dir.join(&name);
        if (target.exists() && target != src) || !targets.insert(target.clone()) {
            return Err(ApiError(AppError::Validation(format!(
                "A file named {name} already exists in the destination"
            ))));
        }
        planned_moves.push((src, target));
    }

    fs::create_dir_all(&dest_dir)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    for (src, target) in planned_moves {
        fs::rename(&src, &target)
            .await
            .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
    }

    Ok(())
}

pub(crate) async fn delete_files(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<DeleteRequest>,
) -> Result<(), ApiError> {
    if req.paths.is_empty() {
        return Err(ApiError(AppError::Validation(
            "At least one path is required".into(),
        )));
    }
    let mut resolved_paths = Vec::with_capacity(req.paths.len());
    for path in &req.paths {
        reject_workspace_root(path, "delete")?;
        let resolved = resolve_file_virtual_path(path, &auth.user_id, &state).await?;
        if !resolved.exists() {
            return Err(ApiError(AppError::NotFound(format!(
                "File not found: {path}"
            ))));
        }
        resolved_paths.push(resolved);
    }

    for resolved in resolved_paths {
        if resolved.is_dir() {
            fs::remove_dir_all(&resolved)
                .await
                .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
        } else {
            fs::remove_file(&resolved)
                .await
                .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
        }
    }
    Ok(())
}

pub(crate) async fn create_user_folder(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<MkdirRequest>,
) -> Result<(), ApiError> {
    let relative = if req.path.starts_with("user://") || req.path.starts_with("agent://") {
        VirtualPath::parse(&req.path)?.relative
    } else {
        req.path.trim_start_matches('/').to_string()
    };
    validate_relative_path(&relative)?;
    let resolved = resolve_file_virtual_path(&req.path, &auth.user_id, &state).await?;

    fs::create_dir_all(&resolved)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    Ok(())
}
