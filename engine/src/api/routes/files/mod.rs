mod browse;
mod models;
mod operations;
mod upload;

use axum::body::Body;
use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tokio::fs;
use tokio_util::io::ReaderStream;

use crate::storage::{VirtualPath, detect_content_type};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::error::AppError;
use crate::core::state::AppState;

use models::{FileAuth, PresignQuery};

const MAX_FILE_SIZE: usize = 10 * 1024 * 1024; // 10MB

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/files", post(upload::upload_file))
        .route("/api/files/presign", post(upload::presign_file))
        .route(
            "/api/files/user/{username}/{*filename}",
            get(browse::download_user_file).delete(browse::delete_user_file),
        )
        .route(
            "/api/files/agent/{agent_id}/{*filepath}",
            get(browse::download_agent_file),
        )
        .route("/api/files/browse/user", get(browse::list_user_files))
        .route("/api/files/browse/user/{*dirpath}", get(browse::list_user_files))
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

        let query: Query<PresignQuery> =
            Query::try_from_uri(&parts.uri)
                .map_err(|_| ApiError(AppError::Auth("Missing authorization".into())))?;

        let token = query
            .presign
            .as_deref()
            .ok_or_else(|| ApiError(AppError::Auth("Missing authorization".into())))?;

        let claims = state.presign_service.verify(token).await?;

        Ok(FileAuth::Presigned {
            owner: claims.owner,
            path: claims.path,
        })
    }
}

pub(super) async fn serve_file(vpath: &VirtualPath, state: &AppState) -> Result<Response, ApiError> {
    let resolved = state.storage_service.resolve(vpath)?;

    if !resolved.exists() {
        return Err(ApiError(AppError::NotFound(
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
