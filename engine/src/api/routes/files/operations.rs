use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::Json;
use tokio::fs;

use crate::storage::{VirtualPath, validate_relative_path};

use super::super::super::error::ApiError;
use super::super::super::middleware::auth::AuthUser;
use super::models::{CopyMoveRequest, MkdirRequest, RenameRequest};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub(crate) async fn rename_user_file(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<RenameRequest>,
) -> Result<(), ApiError> {
    let trimmed = req.path.trim_start_matches('/');
    let vpath = VirtualPath::user(&auth.username, trimmed);
    let resolved = state.storage_service.resolve(&vpath)?;

    if !resolved.exists() {
        return Err(ApiError(AppError::NotFound(
            "File not found".into(),
        )));
    }

    if req.new_name.contains('/') || req.new_name.contains("..") || req.new_name.contains('\0') {
        return Err(ApiError(AppError::Validation(
            "Invalid filename".into(),
        )));
    }

    let dest = resolved
        .parent()
        .ok_or_else(|| {
            ApiError(AppError::Internal("No parent dir".into()))
        })?
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

fn resolve_file_virtual_path(
    path: &str,
    auth: &AuthUser,
    storage: &crate::storage::StorageService,
) -> Result<PathBuf, ApiError> {
    if let Some(rest) = path.strip_prefix("user://") {
        let slash = rest.find('/').unwrap_or(rest.len());
        let path_username = &rest[..slash];
        if path_username != auth.username {
            return Err(ApiError(AppError::Forbidden(
                "Cannot access another user's files".into(),
            )));
        }
        let vpath = VirtualPath::parse(path)?;
        storage.resolve(&vpath).map_err(ApiError)
    } else if path.starts_with("agent://") {
        let vpath = VirtualPath::parse(path)?;
        storage.resolve(&vpath).map_err(ApiError)
    } else {
        let trimmed = path.trim_start_matches('/');
        let vpath = VirtualPath::user(&auth.username, trimmed);
        storage.resolve(&vpath).map_err(ApiError)
    }
}

fn ensure_user_destination(path: &str) -> Result<(), ApiError> {
    if path.starts_with("agent://") {
        return Err(ApiError(AppError::Forbidden(
            "Cannot write to agent workspaces".into(),
        )));
    }
    Ok(())
}

pub(crate) async fn copy_files(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CopyMoveRequest>,
) -> Result<(), ApiError> {
    ensure_user_destination(&req.destination)?;

    let dest_dir =
        resolve_file_virtual_path(&req.destination, &auth, &state.storage_service)?;

    fs::create_dir_all(&dest_dir)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    for source in &req.sources {
        let src = resolve_file_virtual_path(source, &auth, &state.storage_service)?;
        if !src.exists() {
            continue;
        }
        let name = src
            .file_name()
            .ok_or_else(|| {
                ApiError(AppError::Internal("No filename".into()))
            })?
            .to_string_lossy()
            .into_owned();
        let target = dest_dir.join(&name);
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
    ensure_user_destination(&req.destination)?;

    let dest_dir =
        resolve_file_virtual_path(&req.destination, &auth, &state.storage_service)?;

    fs::create_dir_all(&dest_dir)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    for source in &req.sources {
        if source.starts_with("agent://") {
            return Err(ApiError(AppError::Forbidden(
                "Cannot move from agent workspaces".into(),
            )));
        }
        let src = resolve_file_virtual_path(source, &auth, &state.storage_service)?;
        if !src.exists() {
            continue;
        }
        let name = src
            .file_name()
            .ok_or_else(|| {
                ApiError(AppError::Internal("No filename".into()))
            })?
            .to_string_lossy()
            .into_owned();
        let target = dest_dir.join(&name);
        fs::rename(&src, &target)
            .await
            .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
    }

    Ok(())
}

pub(crate) async fn create_user_folder(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<MkdirRequest>,
) -> Result<(), ApiError> {
    let trimmed = req.path.trim_start_matches('/');
    validate_relative_path(trimmed)?;

    let vpath = VirtualPath::user(&auth.username, trimmed);
    let resolved = state.storage_service.resolve(&vpath)?;

    fs::create_dir_all(&resolved)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    Ok(())
}
