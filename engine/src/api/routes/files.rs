use std::path::Path;

use axum::body::Body;
use axum::extract::{FromRequestParts, Multipart, Path as AxumPath, Query, State};
use axum::http::request::Parts;
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::fs;
use tokio_util::io::ReaderStream;

use crate::api::files::{
    Attachment, dedup_filename, detect_content_type, resolve_virtual_path,
};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::state::AppState;

const MAX_FILE_SIZE: usize = 10 * 1024 * 1024; // 10MB

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/files", post(upload_file))
        .route("/api/files/presign", post(presign_file))
        .route(
            "/api/files/user/{username}/{*filename}",
            get(download_user_file).delete(delete_user_file),
        )
        .route(
            "/api/files/agent/{agent_id}/{*filepath}",
            get(download_agent_file),
        )
}

enum FileAuth {
    User(AuthUser),
    Presigned { owner: String, path: String },
}

#[derive(Deserialize)]
struct PresignQuery {
    presign: Option<String>,
}

impl FromRequestParts<AppState> for FileAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Try Bearer auth first
        if let Ok(auth) = AuthUser::from_request_parts(parts, state).await {
            return Ok(FileAuth::User(auth));
        }

        // Fall back to presign query param
        let query: Query<PresignQuery> =
            Query::try_from_uri(&parts.uri)
                .map_err(|_| ApiError(crate::core::error::AppError::Auth("Missing authorization".into())))?;

        let token = query
            .presign
            .as_deref()
            .ok_or_else(|| ApiError(crate::core::error::AppError::Auth("Missing authorization".into())))?;

        let claims = state.presign_service.verify(token).await?;

        Ok(FileAuth::Presigned {
            owner: claims.owner,
            path: claims.path,
        })
    }
}

async fn upload_file(
    auth: AuthUser,
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Attachment>, ApiError> {
    let mut file_data: Option<(String, Vec<u8>)> = None;
    let mut relative_path: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(crate::core::error::AppError::Validation(e.to_string())))?
    {
        match field.name() {
            Some("path") => {
                relative_path = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError(crate::core::error::AppError::Validation(e.to_string())))?,
                );
            }
            Some("file") => {
                let filename = field
                    .file_name()
                    .unwrap_or("upload")
                    .to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError(crate::core::error::AppError::Validation(e.to_string())))?;

                if bytes.len() > MAX_FILE_SIZE {
                    return Err(ApiError(crate::core::error::AppError::Validation(
                        format!("File too large (max {}MB)", MAX_FILE_SIZE / 1024 / 1024),
                    )));
                }

                file_data = Some((filename, bytes.to_vec()));
            }
            _ => {}
        }
    }

    let (original_filename, bytes) = file_data.ok_or_else(|| {
        ApiError(crate::core::error::AppError::Validation(
            "Missing file field".into(),
        ))
    })?;

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
        let dir = Path::new(&state.config.storage.files_path)
            .join(&auth.username)
            .join(&parent);
        (dir, leaf, Some(rel_path.clone()))
    } else {
        let dir = Path::new(&state.config.storage.files_path).join(&auth.username);
        (dir, original_filename.clone(), None)
    };

    fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(e.to_string())))?;

    let final_filename = dedup_filename(&dir, &filename_for_dedup);
    let dest = dir.join(&final_filename);

    fs::write(&dest, &bytes)
        .await
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(e.to_string())))?;

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

    Ok(Json(Attachment {
        filename: final_filename,
        content_type,
        size_bytes,
        owner: format!("user:{}", auth.user_id),
        path: relative,
        url: None,
    }))
}

async fn download_user_file(
    file_auth: FileAuth,
    State(state): State<AppState>,
    AxumPath((username, filename)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    match file_auth {
        FileAuth::User(auth) => {
            if username != auth.username {
                return Err(ApiError(crate::core::error::AppError::Forbidden(
                    "Cannot access another user's files".into(),
                )));
            }
        }
        FileAuth::Presigned { owner, path } => {
            if !owner.starts_with("user:") || path != filename {
                return Err(ApiError(crate::core::error::AppError::Forbidden(
                    "Presigned URL does not match requested file".into(),
                )));
            }
        }
    }

    let virtual_path = format!("user://{username}/{filename}");
    serve_file(&virtual_path, &state).await
}

async fn download_agent_file(
    file_auth: FileAuth,
    State(state): State<AppState>,
    AxumPath((agent_id, filepath)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    if let FileAuth::Presigned { owner, path } = &file_auth
        && (owner != &format!("agent:{agent_id}") || *path != filepath)
    {
        return Err(ApiError(crate::core::error::AppError::Forbidden(
            "Presigned URL does not match requested file".into(),
        )));
    }

    let virtual_path = format!("agent://{agent_id}/{filepath}");
    serve_file(&virtual_path, &state).await
}

async fn delete_user_file(
    auth: AuthUser,
    State(state): State<AppState>,
    AxumPath((username, filename)): AxumPath<(String, String)>,
) -> Result<(), ApiError> {
    if username != auth.username {
        return Err(ApiError(crate::core::error::AppError::Forbidden(
            "Cannot delete another user's files".into(),
        )));
    }

    let virtual_path = format!("user://{username}/{filename}");
    let resolved = resolve_virtual_path(&virtual_path, &state.config)?;

    if !resolved.exists() {
        return Err(ApiError(crate::core::error::AppError::NotFound(
            "File not found".into(),
        )));
    }

    fs::remove_file(&resolved)
        .await
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(e.to_string())))?;

    Ok(())
}

#[derive(Deserialize)]
struct PresignRequest {
    owner: String,
    path: String,
}

async fn presign_file(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<PresignRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Validate ownership: user files must belong to the requesting user
    if let Some(user_id) = req.owner.strip_prefix("user:")
        && user_id != auth.user_id
    {
        return Err(ApiError(crate::core::error::AppError::Forbidden(
            "Cannot presign another user's files".into(),
        )));
    }

    // Resolve the virtual path to verify the file exists
    let virtual_path = if req.owner.starts_with("user:") {
        format!("user://{}/{}", auth.username, req.path)
    } else if let Some(agent_id) = req.owner.strip_prefix("agent:") {
        format!("agent://{}/{}", agent_id, req.path)
    } else {
        return Err(ApiError(crate::core::error::AppError::Validation(
            "Invalid owner prefix".into(),
        )));
    };
    let _ = resolve_virtual_path(&virtual_path, &state.config)?;

    let url = state
        .presign_service
        .sign(&req.owner, &req.path, &auth.user_id, &auth.username)
        .await?;

    let url = if url.is_empty() { None } else { Some(url) };
    Ok(Json(serde_json::json!({ "url": url })))
}

async fn serve_file(virtual_path: &str, state: &AppState) -> Result<Response, ApiError> {
    let resolved = resolve_virtual_path(virtual_path, &state.config)?;

    if !resolved.exists() {
        return Err(ApiError(crate::core::error::AppError::NotFound(
            "File not found".into(),
        )));
    }

    let filename = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let content_type = detect_content_type(filename);

    let file = fs::File::open(&resolved)
        .await
        .map_err(|e| ApiError(crate::core::error::AppError::Internal(e.to_string())))?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{filename}\""),
        )
        .body(body)
        .unwrap())
}

fn validate_relative_path(path: &str) -> Result<(), ApiError> {
    if path.contains("..") {
        return Err(ApiError(crate::core::error::AppError::Validation(
            "Path traversal not allowed".into(),
        )));
    }
    if path.starts_with('/') {
        return Err(ApiError(crate::core::error::AppError::Validation(
            "Path must be relative".into(),
        )));
    }
    Ok(())
}
