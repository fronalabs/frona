mod browse;
mod models;
mod operations;
mod upload;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Query};
use axum::http::header;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::fs;
use tokio_util::io::ReaderStream;

use crate::storage::{VirtualPath, detect_content_type};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::config::ServerConfig;
use crate::core::error::{AppError, AuthErrorCode};
use crate::core::state::AppState;

use models::{FileAuth, PresignQuery};

/// Presentation only: both modes require the same file authorization.
#[derive(Default, Deserialize)]
pub(super) struct FileResponseQuery {
    #[serde(default)]
    pub(super) download: bool,
}

pub fn router(server: &ServerConfig) -> Router<AppState> {
    Router::new()
        .route(
            "/api/files",
            post(upload::upload_file).layer(DefaultBodyLimit::max(
                server.max_upload_request_size_bytes(),
            )),
        )
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
    download: bool,
) -> Result<Response, ApiError> {
    let resolved = state.storage_service.resolve_virtual_path(vpath)?;
    serve_path(&resolved, download).await
}

/// Caller is responsible for path traversal / ownership checks.
pub(super) async fn serve_path(
    path: &std::path::Path,
    download: bool,
) -> Result<Response, ApiError> {
    if !path.exists() {
        return Err(ApiError(AppError::NotFound("File not found".into())));
    }

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let file = fs::File::open(path)
        .await
        .map_err(|e| ApiError(AppError::Internal(e.to_string())))?;
    serve_open_file(file, filename, download)
}

pub(super) fn serve_open_file(
    file: fs::File,
    filename: &str,
    download: bool,
) -> Result<Response, ApiError> {
    let content_type = detect_content_type(filename);
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition(filename, download),
        );
    if matches!(
        content_type,
        "text/html" | "image/svg+xml" | "application/xml"
    ) {
        // Keep dashboards executable, but give documents an opaque origin even
        // on direct/presigned navigation. Never add allow-same-origin here: these
        // files are untrusted and may share a host with the application's session.
        // Embedded/public assets work; workspace siblings need their own signed
        // URLs. This does not grant storage or an authenticated Frona API bridge.
        response = response.header(header::CONTENT_SECURITY_POLICY, "sandbox allow-scripts");
    }
    response
        .body(body)
        .map_err(|error| ApiError(AppError::Internal(error.to_string())))
}

fn content_disposition(filename: &str, download: bool) -> String {
    let disposition = if download { "attachment" } else { "inline" };
    // Path separators and control characters are never useful in a suggested
    // filename, including on clients whose path rules differ from the server's.
    let filename: String = filename
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let fallback: String = filename
        .chars()
        .map(|c| {
            if !c.is_ascii() || matches!(c, '"' | '%') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut value = format!("{disposition}; filename=\"{fallback}\"");
    if filename != fallback {
        // RFC 8187 extended values preserve Unicode/quotes without raw header
        // bytes. Encode everything except unreserved URI bytes (a safe subset of
        // attr-char); spaces must be %20, not form-urlencoded '+'.
        let encoded: String = filename
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        value.push_str(&format!("; filename*=UTF-8''{encoded}"));
    }
    value
}
