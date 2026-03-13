use serde::Deserialize;

use super::super::super::middleware::auth::AuthUser;

pub(super) enum FileAuth {
    User(AuthUser),
    Presigned { owner: String, path: String },
}

#[derive(Deserialize)]
pub(super) struct PresignQuery {
    pub(super) presign: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct PresignRequest {
    pub(super) owner: String,
    pub(super) path: String,
}

#[derive(Deserialize)]
pub(super) struct SearchQuery {
    pub(super) q: String,
    pub(super) scope: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RenameRequest {
    pub(super) path: String,
    pub(super) new_name: String,
}

#[derive(Deserialize)]
pub(super) struct CopyMoveRequest {
    pub(super) sources: Vec<String>,
    pub(super) destination: String,
}

#[derive(Deserialize)]
pub(super) struct MkdirRequest {
    pub(super) path: String,
}
