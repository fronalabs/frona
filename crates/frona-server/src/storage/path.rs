use std::fmt;
use std::path::Path;

use crate::core::error::AppError;

#[derive(Debug, Clone)]
pub enum Namespace {
    User(String),
    Agent(String),
}

#[derive(Debug, Clone)]
pub struct VirtualPath {
    pub namespace: Namespace,
    pub relative: String,
}

impl VirtualPath {
    pub fn parse(s: &str) -> Result<Self, AppError> {
        if let Some(rest) = s.strip_prefix("user://") {
            let (name, rel) = split_first_segment(rest);
            Ok(Self {
                namespace: Namespace::User(name.to_string()),
                relative: rel.to_string(),
            })
        } else if let Some(rest) = s.strip_prefix("agent://") {
            let (name, rel) = split_first_segment(rest);
            Ok(Self {
                namespace: Namespace::Agent(name.to_string()),
                relative: rel.to_string(),
            })
        } else {
            Err(AppError::Validation(format!(
                "Invalid virtual path scheme: {s}"
            )))
        }
    }

    pub fn user(handle: &crate::core::Handle, path: &str) -> Self {
        Self {
            namespace: Namespace::User(handle.as_ref().to_string()),
            relative: path.to_string(),
        }
    }

    pub fn agent(agent_id: &str, path: &str) -> Self {
        Self {
            namespace: Namespace::Agent(agent_id.to_string()),
            relative: path.to_string(),
        }
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            Namespace::User(name) => write!(f, "user://{name}/{}", self.relative),
            Namespace::Agent(name) => write!(f, "agent://{name}/{}", self.relative),
        }
    }
}

fn split_first_segment(s: &str) -> (&str, &str) {
    match s.find('/') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

pub fn validate_no_traversal(resolved: &Path, base: &str) -> Result<(), AppError> {
    for component in resolved.components() {
        if let std::path::Component::ParentDir = component {
            return Err(AppError::Validation("Path traversal not allowed".into()));
        }
    }

    let base_path = Path::new(base);
    if !resolved.starts_with(base_path) {
        return Err(AppError::Validation(
            "Path escapes allowed directory".into(),
        ));
    }
    if !base_path.exists() {
        return Ok(());
    }

    let base_canonical = std::fs::canonicalize(base_path)
        .map_err(|error| AppError::Internal(format!("Failed to resolve storage root: {error}")))?;
    let mut existing_ancestor = resolved.to_path_buf();
    while !existing_ancestor.exists() {
        if !existing_ancestor.pop() {
            return Err(AppError::Validation(
                "Path escapes allowed directory".into(),
            ));
        }
    }
    let ancestor_canonical = std::fs::canonicalize(&existing_ancestor)
        .map_err(|error| AppError::Internal(format!("Failed to resolve path: {error}")))?;

    if !ancestor_canonical.starts_with(&base_canonical) {
        return Err(AppError::Validation(
            "Path escapes allowed directory".into(),
        ));
    }

    Ok(())
}

pub fn validate_relative_path(path: &str) -> Result<(), AppError> {
    if path.contains("..") {
        return Err(AppError::Validation("Path traversal not allowed".into()));
    }
    if path.starts_with('/') {
        return Err(AppError::Validation("Path must be relative".into()));
    }
    if path.contains('\0') {
        return Err(AppError::Validation(
            "Path contains invalid characters".into(),
        ));
    }
    Ok(())
}

pub fn dedup_filename(dir: &Path, filename: &str) -> String {
    let target = dir.join(filename);
    if !target.exists() {
        return filename.to_string();
    }

    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);

    let ext_with_dots = &filename[stem.len()..];

    for i in 1..1000 {
        let candidate = format!("{stem}-{i}{ext_with_dots}");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }

    format!("{stem}-{}{ext_with_dots}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_path() {
        let vp = VirtualPath::parse("user://mina/report.pdf").unwrap();
        assert!(matches!(vp.namespace, Namespace::User(ref n) if n == "mina"));
        assert_eq!(vp.relative, "report.pdf");
    }

    #[test]
    fn parse_agent_path() {
        let vp = VirtualPath::parse("agent://dev/output.csv").unwrap();
        assert!(matches!(vp.namespace, Namespace::Agent(ref n) if n == "dev"));
        assert_eq!(vp.relative, "output.csv");
    }

    #[test]
    fn parse_agent_nested_path() {
        let vp = VirtualPath::parse("agent://dev/subdir/nested/file.txt").unwrap();
        assert!(matches!(vp.namespace, Namespace::Agent(ref n) if n == "dev"));
        assert_eq!(vp.relative, "subdir/nested/file.txt");
    }

    #[test]
    fn parse_invalid_scheme() {
        assert!(VirtualPath::parse("invalid://x/y").is_err());
    }

    #[test]
    fn display_round_trip() {
        let vp = VirtualPath::user(&crate::handle!("mina"), "report.pdf");
        assert_eq!(vp.to_string(), "user://mina/report.pdf");

        let vp = VirtualPath::agent("dev", "output.csv");
        assert_eq!(vp.to_string(), "agent://dev/output.csv");
    }

    #[test]
    fn validate_relative_path_rejects_traversal() {
        assert!(validate_relative_path("../etc/passwd").is_err());
    }

    #[test]
    fn validate_relative_path_rejects_absolute() {
        assert!(validate_relative_path("/etc/passwd").is_err());
    }

    #[test]
    fn validate_relative_path_rejects_null() {
        assert!(validate_relative_path("file\0name").is_err());
    }

    #[test]
    fn validate_relative_path_accepts_valid() {
        assert!(validate_relative_path("subdir/file.txt").is_ok());
    }

    #[test]
    fn dedup_no_conflict() {
        let dir = std::env::temp_dir().join("frona_test_dedup_empty");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(dedup_filename(&dir, "report.pdf"), "report.pdf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_with_conflict() {
        let dir = std::env::temp_dir().join("frona_test_dedup_conflict");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("report.pdf"), b"").unwrap();

        assert_eq!(dedup_filename(&dir, "report.pdf"), "report-1.pdf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_with_multiple_conflicts() {
        let dir = std::env::temp_dir().join("frona_test_dedup_multi");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("report.pdf"), b"").unwrap();
        std::fs::write(dir.join("report-1.pdf"), b"").unwrap();

        assert_eq!(dedup_filename(&dir, "report.pdf"), "report-2.pdf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_compound_extension() {
        let dir = std::env::temp_dir().join("frona_test_dedup_compound");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("archive.tar.gz"), b"").unwrap();

        assert_eq!(dedup_filename(&dir, "archive.tar.gz"), "archive.tar-1.gz");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_no_extension() {
        let dir = std::env::temp_dir().join("frona_test_dedup_noext");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("Makefile"), b"").unwrap();

        assert_eq!(dedup_filename(&dir, "Makefile"), "Makefile-1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
