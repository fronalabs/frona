use std::path::Path;

use axum::Json;
use axum::extract::multipart::MultipartError;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use tokio::fs;

use crate::storage::{
    Attachment, VirtualPath, dedup_filename, detect_content_type, validate_relative_path,
};

use super::super::super::error::ApiError;
use super::super::super::middleware::auth::AuthUser;
use super::models::PresignRequest;
use crate::core::error::AppError;
use crate::core::state::AppState;

fn multipart_error(error: MultipartError, max_request_size: usize) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError(AppError::Http {
            status: StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
            message: format!(
                "Upload request too large (max {max_request_size} bytes including multipart data)"
            ),
        })
    } else {
        ApiError(AppError::Validation(error.body_text()))
    }
}

pub(crate) async fn upload_file(
    auth: AuthUser,
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Attachment>, ApiError> {
    let max_file_size = state.config.server.max_upload_file_size_bytes;
    let map_multipart_error =
        |error| multipart_error(error, state.config.server.max_upload_request_size_bytes());
    let mut file_data: Option<(String, Vec<u8>)> = None;
    let mut relative_path: Option<String> = None;

    while let Some(mut field) = multipart.next_field().await.map_err(map_multipart_error)? {
        match field.name() {
            Some("path") => {
                relative_path = Some(field.text().await.map_err(map_multipart_error)?);
            }
            Some("file") | Some("upload") => {
                let filename = field.file_name().unwrap_or("upload").to_string();
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(map_multipart_error)? {
                    if chunk.len() > max_file_size - bytes.len() {
                        return Err(ApiError(AppError::Http {
                            status: StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
                            message: format!("File too large (max {max_file_size} bytes)"),
                        }));
                    }
                    bytes.extend_from_slice(&chunk);
                }

                file_data = Some((filename, bytes));
            }
            _ => {}
        }
    }

    let (original_filename, bytes) =
        file_data.ok_or_else(|| ApiError(AppError::Validation("Missing file field".into())))?;

    let user_handle = state.user_service.handle_of(&auth.user_id).await?;
    let user_ws = state.storage_service.user_workspace(&user_handle);
    let base = user_ws.base_path().to_path_buf();

    let (dir, filename_for_dedup, virtual_relative) = if let Some(ref rel_path) = relative_path {
        validate_relative_path(rel_path)?;
        let parent = Path::new(rel_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let leaf = Path::new(rel_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&original_filename)
            .to_string();
        let dir = base.join(&parent);
        (dir, leaf, Some(rel_path.clone()))
    } else {
        (base, original_filename.clone(), None)
    };

    fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    let final_filename = dedup_filename(&dir, &filename_for_dedup);
    let dest = dir.join(&final_filename);

    fs::write(&dest, &bytes)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;

    let relative = if let Some(mut rel) = virtual_relative {
        if let Some(parent) = Path::new(&rel).parent() {
            rel = parent.join(&final_filename).to_string_lossy().into_owned();
        } else {
            rel = final_filename.clone();
        }
        rel
    } else {
        final_filename.clone()
    };

    let content_type = detect_content_type(&final_filename).to_string();
    let size_bytes = bytes.len() as u64;
    let owner = format!("user:{}", auth.user_id);
    let url = state
        .presign_service
        .sign(&owner, &relative, &auth.user_id, &user_handle)
        .await
        .ok()
        .filter(|u| !u.is_empty());

    Ok(Json(Attachment {
        filename: final_filename,
        content_type,
        size_bytes,
        owner,
        path: relative,
        url,
    }))
}

pub(crate) async fn presign_file(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<PresignRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_handle = state.user_service.handle_of(&auth.user_id).await?;
    if let Some(user_id) = req.owner.strip_prefix("user:")
        && user_id != auth.user_id
    {
        return Err(ApiError(AppError::Forbidden(
            "Cannot presign another user's files".into(),
        )));
    }

    let vpath = if req.owner.starts_with("user:") {
        VirtualPath::user(&user_handle, &req.path)
    } else if let Some(agent_id) = req.owner.strip_prefix("agent:") {
        let agent = state
            .agent_service
            .owned_by(&auth.user_id, agent_id)
            .await?;
        VirtualPath::agent(agent.handle.as_ref(), &req.path)
    } else {
        return Err(ApiError(AppError::Validation(
            "Invalid owner prefix".into(),
        )));
    };
    let _ = state
        .storage_service
        .resolve_virtual_path_for_user(&user_handle, &vpath)?;

    let url = state
        .presign_service
        .sign(&req.owner, &req.path, &auth.user_id, &user_handle)
        .await?;

    if url.is_empty() {
        return Err(ApiError(AppError::Internal(
            "Failed to generate presigned URL".into(),
        )));
    }

    Ok(Json(serde_json::json!({ "url": url })))
}
