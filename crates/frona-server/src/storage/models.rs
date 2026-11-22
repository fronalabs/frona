use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::core::error::AppError;

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub owner: String,
    pub path: String,
    pub url: Option<String>,
}

pub(crate) fn extend_unique_attachments(
    all_attachments: &mut Vec<Attachment>,
    attachments: &[Attachment],
) {
    for attachment in attachments {
        let already_attached = all_attachments
            .iter()
            .any(|existing| existing.owner == attachment.owner && existing.path == attachment.path);
        if !already_attached {
            all_attachments.push(attachment.clone());
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct PresignClaims {
    pub sub: String,
    pub owner: String,
    pub path: String,
    pub exp: usize,
}

#[derive(Serialize)]
pub struct FileEntry {
    pub id: String,
    pub size: u64,
    pub date: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub parent: String,
}

pub struct SearchTarget {
    pub dir: std::path::PathBuf,
    pub root: std::path::PathBuf,
    pub source: String,
}

pub async fn resolve_workspace_attachment(
    storage: &crate::storage::service::StorageService,
    user_handle: &crate::core::Handle,
    agent_handle: &crate::core::Handle,
    relative_path: &str,
) -> Result<Attachment, AppError> {
    if relative_path.contains("..") {
        return Err(AppError::Validation("Path traversal not allowed".into()));
    }

    let resolved = storage
        .agent_workspace_path(user_handle, agent_handle)
        .join(relative_path);

    if !resolved.exists() {
        return Err(AppError::NotFound(format!(
            "File not found in workspace: {relative_path}"
        )));
    }

    let metadata = tokio::fs::metadata(&resolved)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to read file metadata: {e}")))?;

    let filename = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(relative_path)
        .to_string();

    let content_type = super::detect_content_type(&filename).to_string();

    Ok(Attachment {
        filename,
        content_type,
        size_bytes: metadata.len(),
        owner: format!("agent:{agent_handle}"),
        path: relative_path.to_string(),
        url: None,
    })
}

pub fn attachment_url_segment(
    owner: &str,
    path: &str,
    user_handle: Option<&str>,
) -> Option<String> {
    if owner.starts_with("user:") {
        Some(format!("user/{}/{path}", user_handle?))
    } else {
        owner
            .strip_prefix("agent:")
            .map(|agent_id| format!("agent/{agent_id}/{path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_url_segment_user() {
        assert_eq!(
            attachment_url_segment("user:uid-123", "report.pdf", Some("jdoe")),
            Some("user/jdoe/report.pdf".to_string()),
        );
    }

    #[test]
    fn attachment_url_segment_user_no_handle() {
        assert_eq!(
            attachment_url_segment("user:uid-123", "report.pdf", None),
            None,
        );
    }

    #[test]
    fn attachment_url_segment_agent() {
        assert_eq!(
            attachment_url_segment("agent:developer", "output.csv", None),
            Some("agent/developer/output.csv".to_string()),
        );
    }

    #[test]
    fn attachment_url_segment_agent_nested() {
        assert_eq!(
            attachment_url_segment("agent:developer", "subdir/file.txt", None),
            Some("agent/developer/subdir/file.txt".to_string()),
        );
    }

    #[test]
    fn attachment_url_segment_invalid_owner() {
        assert_eq!(
            attachment_url_segment("unknown:x", "file.txt", Some("jdoe")),
            None,
        );
    }

    #[test]
    fn attachment_url_defaults_to_none() {
        let json = r#"{"filename":"f.txt","content_type":"text/plain","size_bytes":10,"owner":"user:uid","path":"f.txt"}"#;
        let att: Attachment = serde_json::from_str(json).unwrap();
        assert!(att.url.is_none());
        assert_eq!(att.owner, "user:uid");
    }

    #[test]
    fn attachment_url_round_trips() {
        let att = Attachment {
            filename: "f.txt".into(),
            content_type: "text/plain".into(),
            size_bytes: 10,
            owner: "user:uid".into(),
            path: "f.txt".into(),
            url: Some("http://localhost/presigned".into()),
        };
        let json = serde_json::to_string(&att).unwrap();
        assert!(json.contains("\"url\":"));

        let parsed: Attachment = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.url.as_deref(), Some("http://localhost/presigned"));
    }

    #[test]
    fn attachment_url_none_omitted_from_json() {
        let att = Attachment {
            filename: "f.txt".into(),
            content_type: "text/plain".into(),
            size_bytes: 10,
            owner: "user:uid".into(),
            path: "f.txt".into(),
            url: None,
        };
        let json = serde_json::to_string(&att).unwrap();
        assert!(!json.contains("\"url\""));
    }

    #[test]
    fn presign_claims_round_trips() {
        let claims = PresignClaims {
            sub: "uid-123".into(),
            owner: "user:uid-123".into(),
            path: "file.pdf".into(),
            exp: 9999999999,
        };
        let json = serde_json::to_string(&claims).unwrap();
        let parsed: PresignClaims = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.sub, "uid-123");
        assert_eq!(parsed.owner, "user:uid-123");
        assert_eq!(parsed.path, "file.pdf");
        assert_eq!(parsed.exp, 9999999999);
    }
}
