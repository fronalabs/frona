//! Typed file tools for in-workspace work. `produce_file` is the
//! publish-to-chat-as-attachment primitive; these tools are for working
//! with files inside the agent's workspace (and Cedar-permitted siblings).

pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// Virtual-path URIs (`agent://`, `user://`) are deliberately rejected:
/// they have no slot for the owning user, so they cannot address an
/// agent owned by a specific user without separate context.
pub fn resolve_path(
    input: &str,
    user_handle: &crate::core::Handle,
    agent_handle: &crate::core::Handle,
    storage: &crate::storage::service::StorageService,
) -> Result<std::path::PathBuf, crate::core::error::AppError> {
    if input.is_empty() {
        return Err(crate::core::error::AppError::Validation(
            "path must not be empty".into(),
        ));
    }
    if input.starts_with("agent://") || input.starts_with("user://") {
        return Err(crate::core::error::AppError::Validation(format!(
            "virtual-path URIs are not supported here: {input}",
        )));
    }
    // Use the same absolute destination for the existing sandbox-policy check
    // and subsequent I/O. The policy, including explicit sibling grants, owns
    // authorization; resolving a native path does not imply workspace-only access.
    crate::storage::path::resolve_filesystem_path(
        &storage
            .agent_workspace_path(user_handle, agent_handle)
            .join(input),
    )
}

/// Atomically writes `content` to `target` via a tempfile in the same parent.
/// Returns an `AppError` if mkdir, tempfile creation, write, or rename fails.
pub async fn atomic_write(
    target: &std::path::Path,
    content: &[u8],
) -> Result<(), crate::core::error::AppError> {
    let parent = target.parent().ok_or_else(|| {
        crate::core::error::AppError::Validation(format!(
            "path has no parent directory: {}",
            target.display()
        ))
    })?;
    tokio::fs::create_dir_all(parent).await.map_err(|e| {
        crate::core::error::AppError::Internal(format!("mkdir {}: {e}", parent.display()))
    })?;
    let target = target.to_path_buf();
    let content = content.to_vec();
    tokio::task::spawn_blocking(move || -> Result<(), crate::core::error::AppError> {
        let mut tmp = tempfile::NamedTempFile::new_in(target.parent().unwrap())
            .map_err(|e| crate::core::error::AppError::Internal(format!("tempfile: {e}")))?;
        use std::io::Write;
        tmp.write_all(&content)
            .map_err(|e| crate::core::error::AppError::Internal(format!("tempfile write: {e}")))?;
        tmp.persist(&target).map_err(|e| {
            crate::core::error::AppError::Internal(format!(
                "atomic persist to {}: {e}",
                target.display()
            ))
        })?;
        Ok(())
    })
    .await
    .map_err(|e| crate::core::error::AppError::Internal(format!("join: {e}")))??;
    Ok(())
}

#[cfg(test)]
mod resolve_path_tests {
    use super::*;
    use crate::core::Handle;
    use crate::core::config::Config;
    use crate::storage::service::StorageService;
    use std::path::PathBuf;

    fn test_storage(data_dir: &str) -> StorageService {
        let mut cfg = Config::default();
        cfg.storage.data_dir = data_dir.to_string();
        StorageService::new(&cfg)
    }

    fn handle(s: &str) -> Handle {
        Handle::try_new(s).unwrap()
    }

    fn run(
        input: &str,
        user: &str,
        agent: &str,
        data_dir: &str,
    ) -> Result<PathBuf, crate::core::error::AppError> {
        resolve_path(
            input,
            &handle(user),
            &handle(agent),
            &test_storage(data_dir),
        )
    }

    #[test]
    fn bare_path_resolves_to_calling_agents_workspace() {
        // Regression: agent="system", user="mina". A bare path must resolve
        // to `users/mina/agents/system/...`, NOT `users/system/agents/system/...`.
        assert_eq!(
            run("notes.md", "mina", "system", "/app/data").unwrap(),
            PathBuf::from("/app/data/users/mina/agents/system/notes.md")
        );
    }

    #[test]
    fn bare_path_with_subdir() {
        assert_eq!(
            run("subdir/notes.md", "mina", "system", "/app/data").unwrap(),
            PathBuf::from("/app/data/users/mina/agents/system/subdir/notes.md")
        );
    }

    #[test]
    fn absolute_path_is_resolved() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            run(file.path().to_str().unwrap(), "mina", "system", "/app/data").unwrap(),
            std::fs::canonicalize(file.path()).unwrap()
        );
    }

    #[test]
    fn agent_uri_is_rejected() {
        assert!(run("agent://system/notes.md", "mina", "system", "/app/data").is_err());
    }

    #[test]
    fn user_uri_is_rejected() {
        assert!(run("user://mina/notes.md", "mina", "system", "/app/data").is_err());
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(run("", "mina", "system", "/app/data").is_err());
    }

    #[test]
    fn parent_paths_resolve_before_existing_policy_authorizes_them() {
        assert_eq!(
            run("reports/../file.txt", "mina", "system", "/app/data").unwrap(),
            PathBuf::from("/app/data/users/mina/agents/system/file.txt")
        );
        assert_eq!(
            run(
                "missing/../../sibling/file.txt",
                "mina",
                "system",
                "/app/data"
            )
            .unwrap(),
            PathBuf::from("/app/data/users/mina/agents/sibling/file.txt")
        );
    }

    #[test]
    fn bare_path_does_not_collapse_user_into_agent_handle() {
        // Regression: user=alice, agent=researcher. Resolved path must contain
        // BOTH handles in their proper positions, never doubling the agent handle.
        let resolved = run("output.txt", "alice", "researcher", "/data").unwrap();
        let s = resolved.to_string_lossy();
        assert!(s.contains("/users/alice/"), "missing user: {s}");
        assert!(s.contains("/agents/researcher/"), "missing agent: {s}");
        assert!(!s.contains("/users/researcher/"), "user-as-agent: {s}");
    }
}
