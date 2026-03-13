use std::path::{Path, PathBuf};

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

use crate::storage::{
    Attachment, FileEntry, SearchTarget, VirtualPath,
    dedup_filename, detect_content_type, validate_relative_path,
};

use super::super::error::ApiError;
use super::super::middleware::auth::AuthUser;
use crate::core::error::AppError;
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
        .route("/api/files/browse/user", get(list_user_files))
        .route("/api/files/browse/user/{*dirpath}", get(list_user_files))
        .route(
            "/api/files/browse/agent/{agent_id}",
            get(list_agent_files_root),
        )
        .route(
            "/api/files/browse/agent/{agent_id}/{*dirpath}",
            get(list_agent_files_subdir),
        )
        .route("/api/files/search", get(search_files))
        .route("/api/files/rename", post(rename_user_file))
        .route("/api/files/copy", post(copy_files))
        .route("/api/files/move", post(move_files))
        .route("/api/files/mkdir", post(create_user_folder))
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
        .map_err(|e| ApiError(AppError::Validation(e.to_string())))?
    {
        match field.name() {
            Some("path") => {
                relative_path = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError(AppError::Validation(e.to_string())))?,
                );
            }
            Some("file") | Some("upload") => {
                let filename = field
                    .file_name()
                    .unwrap_or("upload")
                    .to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError(AppError::Validation(e.to_string())))?;

                if bytes.len() > MAX_FILE_SIZE {
                    return Err(ApiError(AppError::Validation(
                        format!("File too large (max {}MB)", MAX_FILE_SIZE / 1024 / 1024),
                    )));
                }

                file_data = Some((filename, bytes.to_vec()));
            }
            _ => {}
        }
    }

    let (original_filename, bytes) = file_data.ok_or_else(|| {
        ApiError(AppError::Validation(
            "Missing file field".into(),
        ))
    })?;

    let user_ws = state.storage_service.user_workspace(&auth.username);
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
    serve_file(&vpath, &state).await
}

async fn download_agent_file(
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
    serve_file(&vpath, &state).await
}

async fn delete_user_file(
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
    if let Some(user_id) = req.owner.strip_prefix("user:")
        && user_id != auth.user_id
    {
        return Err(ApiError(AppError::Forbidden(
            "Cannot presign another user's files".into(),
        )));
    }

    let vpath = if req.owner.starts_with("user:") {
        VirtualPath::user(&auth.username, &req.path)
    } else if let Some(agent_id) = req.owner.strip_prefix("agent:") {
        VirtualPath::agent(agent_id, &req.path)
    } else {
        return Err(ApiError(AppError::Validation(
            "Invalid owner prefix".into(),
        )));
    };
    let _ = state.storage_service.resolve(&vpath)?;

    let url = state
        .presign_service
        .sign(&req.owner, &req.path, &auth.user_id, &auth.username)
        .await?;

    if url.is_empty() {
        return Err(ApiError(AppError::Internal(
            "Failed to generate presigned URL".into(),
        )));
    }

    Ok(Json(serde_json::json!({ "url": url })))
}

async fn serve_file(vpath: &VirtualPath, state: &AppState) -> Result<Response, ApiError> {
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

async fn list_user_files(
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

async fn list_agent_files_root(
    auth: AuthUser,
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    list_agent_dir(&auth, &state, &agent_id, "").await
}

async fn list_agent_files_subdir(
    auth: AuthUser,
    State(state): State<AppState>,
    AxumPath((agent_id, dirpath)): AxumPath<(String, String)>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    list_agent_dir(&auth, &state, &agent_id, &dirpath).await
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    scope: Option<String>,
}

async fn search_files(
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

#[derive(Deserialize)]
struct RenameRequest {
    path: String,
    new_name: String,
}

async fn rename_user_file(
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

#[derive(Deserialize)]
struct CopyMoveRequest {
    sources: Vec<String>,
    destination: String,
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

async fn copy_files(
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

async fn move_files(
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

#[derive(Deserialize)]
struct MkdirRequest {
    path: String,
}

async fn create_user_folder(
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
