mod browse;
mod models;
mod operations;
mod upload;

use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequestParts, Query};
use axum::http::header;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::{get, post};
use tokio::fs;
use tokio_util::io::ReaderStream;

use crate::storage::{VirtualPath, detect_content_type};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::error::{AppError, AuthErrorCode};
use crate::core::state::AppState;

use models::{FileAuth, PresignQuery};

const MAX_FILE_SIZE: usize = 10 * 1024 * 1024; // 10MB

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/files", post(upload::upload_file))
        .route("/api/files/presign", post(upload::presign_file))
        .route(
            "/api/files/user/{handle}/{*filename}",
            get(browse::download_user_file).delete(browse::delete_user_file),
        )
        .route(
            "/api/files/agent/{agent_id}/{*filepath}",
            get(browse::download_agent_file),
        )
        .route("/api/files/browse/user", get(browse::list_user_files))
        .route(
            "/api/files/browse/user/{*dirpath}",
            get(browse::list_user_files),
        )
        .route(
            "/api/files/browse/agent/{agent_id}",
            get(browse::list_agent_files_root),
        )
        .route(
            "/api/files/browse/agent/{agent_id}/{*dirpath}",
            get(browse::list_agent_files_subdir),
        )
        .route("/api/files/search", get(browse::search_files))
        .route("/api/files/rename", post(operations::rename_user_file))
        .route("/api/files/copy", post(operations::copy_files))
        .route("/api/files/move", post(operations::move_files))
        .route("/api/files/delete", post(operations::delete_files))
        .route("/api/files/mkdir", post(operations::create_user_folder))
}

impl FromRequestParts<AppState> for FileAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Ok(auth) = AuthUser::from_request_parts(parts, state).await {
            return Ok(FileAuth::User(auth));
        }

        let query: Query<PresignQuery> = Query::try_from_uri(&parts.uri).map_err(|_| {
            ApiError(AppError::Auth {
                message: "Missing authorization".into(),
                code: AuthErrorCode::InvalidCredentials,
            })
        })?;

        let token = query.presign.as_deref().ok_or_else(|| {
            ApiError(AppError::Auth {
                message: "Missing authorization".into(),
                code: AuthErrorCode::InvalidCredentials,
            })
        })?;

        let claims = state.presign_service.verify(token).await?;

        Ok(FileAuth::Presigned {
            sub: claims.sub,
            owner: claims.owner,
            path: claims.path,
        })
    }
}

pub(super) async fn serve_file(
    vpath: &VirtualPath,
    state: &AppState,
) -> Result<Response, ApiError> {
    let resolved = state.storage_service.resolve_virtual_path(vpath)?;
    serve_path(&resolved).await
}

/// Caller is responsible for path traversal / ownership checks.
pub(super) async fn serve_path(path: &std::path::Path) -> Result<Response, ApiError> {
    if !path.exists() {
        return Err(ApiError(AppError::NotFound("File not found".into())));
    }

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let content_type = detect_content_type(filename);

    let file = fs::File::open(path)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
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
