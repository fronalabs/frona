use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::core::Handle;
use crate::core::config::Config;
use crate::core::error::AppError;

use super::models::{FileEntry, SearchTarget};
use super::path::{Namespace, VirtualPath, validate_no_traversal};
use super::workspace::Workspace;

/// Per-user state at `{data_dir}/users/{user_handle}/{subsystem}/...`;
/// shared/template state at `{data_dir}/system/...` or under `shared_config_dir`.
#[derive(Clone)]
pub struct StorageService {
    data_dir: PathBuf,
    shared_agents_dir: PathBuf,
}

impl StorageService {
    pub fn new(config: &Config) -> Self {
        let shared_config_dir = PathBuf::from(&config.storage.shared_config_dir);
        Self {
            data_dir: PathBuf::from(&config.storage.data_dir),
            shared_agents_dir: shared_config_dir.join("agents"),
        }
    }

    pub fn users_root(&self) -> PathBuf {
        self.data_dir.join("users")
    }

    pub fn user_root(&self, user_handle: &Handle) -> PathBuf {
        self.users_root().join(user_handle.as_ref())
    }

    /// Raw path. Use [`Self::agent_workspace`] for the layered Workspace.
    pub fn agent_workspace_path(&self, user_handle: &Handle, agent_handle: &Handle) -> PathBuf {
        self.user_root(user_handle)
            .join("agents")
            .join(agent_handle.as_ref())
    }

    pub fn mcp_workspace_path(&self, user_handle: &Handle, mcp_handle: &Handle) -> PathBuf {
        self.user_root(user_handle)
            .join("mcps")
            .join(mcp_handle.as_ref())
    }

    pub fn user_files_path(&self, user_handle: &Handle) -> PathBuf {
        self.user_root(user_handle).join("files")
    }

    pub fn channel_data_path(&self, user_handle: &Handle, channel_handle: &Handle) -> PathBuf {
        self.user_root(user_handle)
            .join("channels")
            .join(channel_handle.as_ref())
    }

    pub fn browser_profile_path(&self, user_handle: &Handle, provider: &str) -> PathBuf {
        self.user_root(user_handle).join("browser").join(provider)
    }

    pub fn user_vault_path(&self, user_handle: &Handle) -> PathBuf {
        self.user_root(user_handle).join("vault")
    }

    pub fn user_skills_path(&self, user_handle: &Handle) -> PathBuf {
        self.user_root(user_handle).join("skills")
    }

    /// Root of the wiki memory projection - `<handle>/pkm` (sibling of `files/`,
    /// `skills/`). Holds the `<vault-directory>/` tree of markdown pages. Named
    /// `pkm` (not `memory`) so the on-disk path doesn't read `pkm/Memory` as
    /// `memory/Memory` - the leaf directory is the user-configured vault name.
    pub fn user_pkm_path(&self, user_handle: &Handle) -> PathBuf {
        self.user_root(user_handle).join("pkm")
    }

    /// True when `candidate` is below this user's PKM storage directory.
    /// The data root can differ from the active configuration because persisted tool
    /// calls can outlive a database restore, relocation, or benchmark copy.
    pub fn is_user_pkm_path(&self, user_handle: &Handle, candidate: &str) -> bool {
        let expected_root = self.user_pkm_path(user_handle);
        let Ok(expected_relative) = expected_root.strip_prefix(&self.data_dir) else {
            return false;
        };
        let expected = expected_relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut actual = Vec::new();
        for component in Path::new(candidate).components() {
            match component {
                std::path::Component::Normal(value) => actual.push(value),
                std::path::Component::ParentDir => return false,
                _ => {}
            }
        }
        actual
            .windows(expected.len())
            .enumerate()
            .any(|(index, window)| {
                window == expected.as_slice() && index + expected.len() < actual.len()
            })
    }

    pub fn user_tokens_path(&self, user_handle: &Handle) -> PathBuf {
        self.user_root(user_handle).join("tokens")
    }

    pub fn system_root(&self) -> PathBuf {
        self.data_dir.join("system")
    }

    /// Writes go to the per-user override; reads fall back to the shared template.
    pub fn agent_workspace(&self, user_handle: &Handle, agent_handle: &Handle) -> Workspace {
        let workspace_path = self.agent_workspace_path(user_handle, agent_handle);
        let shared_path = self.shared_agents_dir.join(agent_handle.as_ref());
        Workspace::new(vec![workspace_path], Some(shared_path))
    }

    pub fn builtin_template_workspace(&self, handle: &Handle) -> Workspace {
        let shared_path = self.shared_agents_dir.join(handle.as_ref());
        Workspace::new(vec![shared_path.clone()], Some(shared_path))
    }

    pub fn user_workspace(&self, handle: &Handle) -> Workspace {
        Workspace::new(vec![self.user_files_path(handle)], None)
    }

    pub fn resolve(&self, path: &str) -> Result<PathBuf, AppError> {
        if path.starts_with('/') {
            return Ok(PathBuf::from(path));
        }
        let parsed = VirtualPath::parse(path)?;
        self.resolve_virtual_path(&parsed)
    }

    pub fn resolve_for_user(&self, user_handle: &Handle, path: &str) -> Result<PathBuf, AppError> {
        if path.starts_with('/') {
            return Ok(PathBuf::from(path));
        }
        let parsed = VirtualPath::parse(path)?;
        self.resolve_virtual_path_for_user(user_handle, &parsed)
    }

    pub fn resolve_virtual_path(&self, path: &VirtualPath) -> Result<PathBuf, AppError> {
        let Namespace::User(name) = &path.namespace else {
            return Err(AppError::Validation(
                "agent virtual paths require a user handle".into(),
            ));
        };
        let handle = Handle::try_new(name)?;
        self.resolve_virtual_path_for_user(&handle, path)
    }

    pub fn resolve_virtual_path_for_user(
        &self,
        user_handle: &Handle,
        path: &VirtualPath,
    ) -> Result<PathBuf, AppError> {
        let users_root = self.users_root();
        let users_root_str = users_root.to_string_lossy().into_owned();
        let resolved = match &path.namespace {
            Namespace::User(name) => {
                let handle = Handle::try_new(name)?;
                if &handle != user_handle {
                    return Err(AppError::Validation(
                        "user virtual path does not match the resolving user".into(),
                    ));
                }
                let resolved = self.user_files_path(&handle).join(&path.relative);
                validate_no_traversal(&resolved, &users_root_str)?;
                resolved
            }
            Namespace::Agent(name) => {
                let agent_handle = Handle::try_new(name)?;
                let resolved = self
                    .agent_workspace_path(user_handle, &agent_handle)
                    .join(&path.relative);
                validate_no_traversal(&resolved, &users_root_str)?;
                resolved
            }
        };

        if resolved.is_absolute() {
            Ok(resolved)
        } else {
            Ok(std::env::current_dir()
                .map(|cwd| cwd.join(&resolved))
                .unwrap_or(resolved))
        }
    }

    pub async fn list_dir(&self, dir: &Path, parent_id: &str) -> Result<Vec<FileEntry>, AppError> {
        if !dir.exists() {
            return Ok(vec![]);
        }

        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?
        {
            let metadata = entry
                .metadata()
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?;

            let name = entry.file_name().to_string_lossy().into_owned();
            let id = if parent_id.is_empty() || parent_id == "/" {
                format!("/{name}")
            } else {
                format!("{parent_id}/{name}")
            };

            let modified: DateTime<Utc> = metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                .into();

            entries.push(FileEntry {
                id,
                size: if metadata.is_dir() { 0 } else { metadata.len() },
                date: modified.to_rfc3339(),
                entry_type: if metadata.is_dir() {
                    "folder".into()
                } else {
                    "file".into()
                },
                parent: if parent_id.is_empty() {
                    "/".into()
                } else {
                    parent_id.into()
                },
            });
        }

        Ok(entries)
    }

    pub async fn search(
        &self,
        targets: Vec<SearchTarget>,
        query: &str,
    ) -> Result<Vec<FileEntry>, AppError> {
        let q = query.to_lowercase();

        tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();
            for target in &targets {
                results.extend(search_dir(&target.dir, &target.root, &q, &target.source));
            }
            results
        })
        .await
        .map_err(|e| AppError::Internal(e.to_string()))
    }
}

fn search_dir(dir: &Path, root: &Path, query: &str, source: &str) -> Vec<FileEntry> {
    let mut results = Vec::new();

    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .build();

    for entry in walker.flatten() {
        if entry.path() == dir {
            continue;
        }

        let name = entry.file_name().to_string_lossy();
        if !name.to_lowercase().contains(query) {
            continue;
        }

        let path = entry.path();
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let id = format!("/{rel}");
        let parent_path = path.parent().unwrap_or(root);
        let parent_rel = parent_path
            .strip_prefix(root)
            .unwrap_or(parent_path)
            .to_string_lossy()
            .into_owned();
        let parent = if parent_rel.is_empty() {
            "/".to_string()
        } else {
            format!("/{parent_rel}")
        };

        let metadata = entry.metadata().ok();
        let modified: DateTime<Utc> = metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .into();

        results.push(FileEntry {
            id: format!("{source}:{id}"),
            size: if is_dir {
                0
            } else {
                metadata.as_ref().map(|m| m.len()).unwrap_or(0)
            },
            date: modified.to_rfc3339(),
            entry_type: if is_dir { "folder" } else { "file" }.into(),
            parent: format!("{source}:{parent}"),
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::default()
    }

    fn test_service() -> StorageService {
        StorageService::new(&test_config())
    }

    #[test]
    fn resolve_user_path() {
        let svc = test_service();
        let vp = VirtualPath::user(&crate::handle!("uid-123"), "report.pdf");
        let result = svc.resolve_virtual_path(&vp).unwrap();
        assert!(result.is_absolute());
        assert!(result.ends_with("data/users/uid-123/files/report.pdf"));
    }

    #[test]
    fn resolve_agent_path() {
        let svc = test_service();
        let vp = VirtualPath::agent("dev", "output.csv");
        let result = svc
            .resolve_virtual_path_for_user(&crate::handle!("test-user"), &vp)
            .unwrap();
        assert!(result.is_absolute());
        assert!(result.ends_with("data/users/test-user/agents/dev/output.csv"));
    }

    #[test]
    fn resolve_agent_nested_path() {
        let svc = test_service();
        let vp = VirtualPath::agent("dev", "subdir/nested/file.txt");
        let result = svc
            .resolve_virtual_path_for_user(&crate::handle!("test-user"), &vp)
            .unwrap();
        assert!(result.is_absolute());
        assert!(result.ends_with("data/users/test-user/agents/dev/subdir/nested/file.txt"));
    }

    #[test]
    fn resolve_agent_path_without_user_is_rejected() {
        let svc = test_service();
        let vp = VirtualPath::agent("dev", "output.csv");
        assert!(svc.resolve_virtual_path(&vp).is_err());
    }

    #[test]
    fn resolve_path_traversal_rejected() {
        let svc = test_service();
        let vp = VirtualPath::user(&crate::handle!("uid"), "../../../etc/passwd");
        let result = svc.resolve_virtual_path(&vp);
        assert!(result.is_err());
    }

    fn shared_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("resources")
    }

    #[test]
    fn workspace_read_builtin() {
        let tmp = std::env::temp_dir().join("frona_storage_test_read_builtin");
        let mut config = Config::default();
        config.storage.data_dir = tmp.to_string_lossy().into_owned();
        config.storage.shared_config_dir = shared_dir().to_string_lossy().into_owned();
        let svc = StorageService::new(&config);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("system"));
        let content = ws.read("AGENT.md");
        assert!(content.is_some(), "Should read AGENT.md from shared layer");
        assert!(content.unwrap().contains("You're not a chatbot"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn workspace_read_not_found() {
        let tmp = std::env::temp_dir().join("frona_storage_test_read_not_found");
        let mut config = Config::default();
        config.storage.data_dir = tmp.to_string_lossy().into_owned();
        config.storage.shared_config_dir = shared_dir().to_string_lossy().into_owned();
        let svc = StorageService::new(&config);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("system"));
        assert!(ws.read("nonexistent.md").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn workspace_data_layer_shadows_builtin() {
        let tmp = std::env::temp_dir().join("frona_storage_test_shadow");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut config = Config::default();
        config.storage.data_dir = tmp.to_string_lossy().into_owned();
        config.storage.shared_config_dir = shared_dir().to_string_lossy().into_owned();
        let svc = StorageService::new(&config);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("system"));

        ws.write("AGENT.md", "Custom prompt").unwrap();
        let content = ws.read("AGENT.md").unwrap();
        assert_eq!(content, "Custom prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn workspace_write_creates_file() {
        let tmp = std::env::temp_dir().join("frona_storage_test_write");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut config = Config::default();
        config.storage.data_dir = tmp.to_string_lossy().into_owned();
        config.storage.shared_config_dir = shared_dir().to_string_lossy().into_owned();
        let svc = StorageService::new(&config);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("test_agent"));

        ws.write("test.md", "hello").unwrap();
        assert_eq!(ws.read("test.md").unwrap(), "hello");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn workspace_exists_builtin() {
        let tmp = std::env::temp_dir().join("frona_storage_test_exists_builtin");
        let mut config = Config::default();
        config.storage.data_dir = tmp.to_string_lossy().into_owned();
        config.storage.shared_config_dir = shared_dir().to_string_lossy().into_owned();
        let svc = StorageService::new(&config);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("system"));
        assert!(ws.exists("AGENT.md"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn workspace_read_dir_builtin() {
        let tmp = std::env::temp_dir().join("frona_storage_test_read_dir_builtin");
        let mut config = Config::default();
        config.storage.data_dir = tmp.to_string_lossy().into_owned();
        config.storage.shared_config_dir = shared_dir().to_string_lossy().into_owned();
        let svc = StorageService::new(&config);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("system"));
        let entries = ws.read_dir("");
        assert!(entries.contains(&"AGENT.md".to_string()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn user_workspace_no_shared_dir() {
        let svc = test_service();
        let ws = svc.user_workspace(&crate::handle!("testuser"));
        assert_eq!(ws.base_path(), Path::new("data/users/testuser/files"));
    }
}
