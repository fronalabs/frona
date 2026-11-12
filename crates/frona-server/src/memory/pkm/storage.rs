//! Where a page's file goes - the filesystem seam under `<handle>/pkm`.
//!
//! Everything is a **page** at `<handle>/pkm/<vault-dir>/<path>.md` (the `pkm/`
//! dir is the page tree root; its `<vault-dir>/` leaf is the Obsidian vault -
//! no `pages/` or `playbooks/` subdirs).
//! Derived projections written by the background process; the agent never writes
//! them.
//!
//! This module owns filesystem paths and validates relative paths before I/O.
//! Markdown composition and proposed-path normalization belong to `frona_pkm`.

use std::io::{BufReader, Write};
use std::path::PathBuf;

use crate::core::Handle;
use crate::core::error::AppError;
use crate::storage::StorageService;
use crate::storage::path::validate_relative_path;

use super::page::projection::parse_uid;
use super::vault::VaultScope;

#[derive(Clone)]
pub struct PkmStorage {
    storage: StorageService,
}

impl PkmStorage {
    pub fn new(storage: StorageService) -> Self {
        Self { storage }
    }

    /// The vault-mirror root - `<handle>/pkm` (the server-internal root; the
    /// user-visible Memory directory and any User Vault directories live under it, and it
    /// is the agent's absolute navigation root).
    fn root_dir(&self, handle: &Handle) -> PathBuf {
        self.storage.user_pkm_path(handle)
    }

    /// Validate a memory-directory name: a single clean path segment (no `/`, no
    /// `..`, no NUL). This is the write backstop - the memory service already
    /// validates on the way in, but `memory_dir` re-checks at the point of use, so
    /// even a bad value that somehow reached storage (a future settings writer that
    /// skipped validation, or direct DB tampering) can't `join` its way out of the
    /// user's memory root. Returns the trimmed segment.
    pub(crate) fn validate_directory(directory: &str) -> Result<&str, AppError> {
        let d = directory.trim();
        if d.is_empty() || d.contains('/') || d.contains("..") || d.contains('\0') {
            return Err(AppError::Validation(
                "memory directory must be a single clean segment (no '/' or '..')".into(),
            ));
        }
        Ok(d)
    }

    /// The Memory directory on disk - `<root>/<directory>`. Memory pages map under it at
    /// `<path>.md`. `directory` is the per-user configured name (default `Memory`);
    /// it's the only directory that surfaces in vault paths/wikilinks. Every read/write
    /// path goes through here, and `directory` is [`validate_directory`]-checked here,
    /// so writes are structurally scoped to the Memory directory (the write-scope
    /// guard - a User Vault path can't be written, and a `..` can't escape the root).
    fn memory_dir(&self, scope: &VaultScope) -> Result<PathBuf, AppError> {
        // Re-validated here even though `VaultScope::new` already did: this is the write
        // backstop, so a bad value that reached storage by some path that skipped
        // validation still cannot `join` its way out of the user's root.
        let dir = Self::validate_directory(scope.directory())?;
        Ok(self.root_dir(scope.handle()).join(dir))
    }

    /// The agent's absolute navigation root - the vault-mirror dir. Stated in the
    /// memory system prompt so the agent can turn a vault-relative path `X` into a
    /// file: `read(<root>/X.md)`. Made absolute with `std::path::absolute` (cwd-based,
    /// no filesystem access) so the `read`/sandbox paths resolve even when `data_dir`
    /// is configured relative (e.g. the default `data`).
    /// A [`VaultScope`] for this handle + Memory directory, rooted at [`memory_root`].
    pub fn vault_scope(&self, handle: Handle, directory: &str) -> Result<VaultScope, AppError> {
        let root = self.memory_root(&handle);
        VaultScope::new(handle, directory, root)
    }

    pub fn memory_root(&self, handle: &Handle) -> PathBuf {
        let root = self.root_dir(handle);
        std::path::absolute(&root).unwrap_or(root)
    }

    pub(crate) fn is_user_pkm_path(&self, handle: &Handle, path: &str) -> bool {
        self.storage.is_user_pkm_path(handle, path)
    }

    /// Mirror a User Vault note - the server's read-only copy of a note the plugin
    /// uploaded, at its full vault-relative path **outside** the Memory directory
    /// (under the mirror root). Distinct from `write_page` (the agent's
    /// Memory-directory-scoped write): this is the External-ingest cache, so the agent
    /// can `read`/`grep`/cite the note. The system never pushes it back to the user.
    pub fn write_user_note(
        &self,
        handle: &Handle,
        vault_path: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let rel = Self::rel_md(vault_path)?;
        Self::write_file(self.root_dir(handle), &rel, content)
    }

    /// Remove a User Vault note mirror (on External delete). No-op if absent.
    pub fn delete_user_note(&self, handle: &Handle, vault_path: &str) -> Result<(), AppError> {
        let rel = Self::rel_md(vault_path)?;
        let full = self.root_dir(handle).join(&rel);
        if full.exists() {
            std::fs::remove_file(&full)
                .map_err(|e| AppError::Internal(format!("user note delete: {e}")))?;
        }
        Ok(())
    }

    /// Normalise a page path into a safe relative `<path>.md` (no `..` escape).
    fn rel_md(path: &str) -> Result<String, AppError> {
        let cleaned = path
            .trim()
            .trim_start_matches('/')
            .trim_end_matches(".md")
            .trim_matches('/');
        if cleaned.is_empty() {
            return Err(AppError::Validation("empty memory path".into()));
        }
        validate_relative_path(cleaned)?;
        Ok(format!("{cleaned}.md"))
    }

    fn write_file(dir: PathBuf, rel: &str, content: &str) -> Result<(), AppError> {
        let full = dir.join(rel);
        let parent = full.parent().ok_or_else(|| {
            AppError::Internal(format!(
                "pkm storage: path has no parent: {}",
                full.display()
            ))
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Internal(format!("pkm storage: mkdir: {e}")))?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| AppError::Internal(format!("pkm storage: stage: {e}")))?;
        staged
            .write_all(content.as_bytes())
            .map_err(|e| AppError::Internal(format!("pkm storage: stage write: {e}")))?;
        staged
            .as_file_mut()
            .sync_all()
            .map_err(|e| AppError::Internal(format!("pkm storage: stage sync: {e}")))?;
        staged
            .persist(&full)
            .map_err(|e| AppError::Internal(format!("pkm storage: replace: {}", e.error)))?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| AppError::Internal(format!("pkm storage: directory sync: {e}")))?;
        Ok(())
    }

    /// Reads a vault file relative to `dir`; `None` if absent/unreadable.
    fn read_file(dir: PathBuf, rel: &str) -> Option<String> {
        std::fs::read_to_string(dir.join(rel)).ok()
    }

    pub fn write_page(
        &self,
        scope: &VaultScope,
        path: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let rel = Self::rel_md(path)?;
        Self::write_file(self.memory_dir(scope)?, &rel, content)
    }

    pub fn read_page(&self, scope: &VaultScope, path: &str) -> Option<String> {
        let rel = Self::rel_md(path).ok()?;
        Self::read_file(self.memory_dir(scope).ok()?, &rel)
    }

    pub fn delete_page(&self, scope: &VaultScope, path: &str) -> Result<(), AppError> {
        let rel = Self::rel_md(path)?;
        let full = self.memory_dir(scope)?.join(&rel);
        if full.exists() {
            std::fs::remove_file(&full)
                .map_err(|e| AppError::Internal(format!("pkm storage: page delete: {e}")))?;
        }
        Ok(())
    }

    pub(crate) fn delete_memory_directory(&self, scope: &VaultScope) -> Result<(), AppError> {
        let directory = self.memory_dir(scope)?;
        let metadata = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "pkm storage: inspect memory directory {}: {error}",
                    directory.display()
                )));
            }
        };
        if metadata.file_type().is_symlink() {
            std::fs::remove_file(&directory).map_err(|error| {
                AppError::Internal(format!(
                    "pkm storage: remove memory directory link {}: {error}",
                    directory.display()
                ))
            })?;
        } else {
            std::fs::remove_dir_all(&directory).map_err(|error| {
                AppError::Internal(format!(
                    "pkm storage: remove memory directory {}: {error}",
                    directory.display()
                ))
            })?;
        }
        Ok(())
    }

    /// Move a page's `.md` file from `from` to `to` (preserving its content), for a
    /// rename. No-op if the source file doesn't exist. The moved file's frontmatter
    /// still names the old path until the page is next authored - cosmetic, re-derived.
    pub fn move_page_file(&self, scope: &VaultScope, from: &str, to: &str) -> Result<(), AppError> {
        if let Some(content) = self.read_page(scope, from) {
            self.write_page(scope, to, &content)?;
            self.delete_page(scope, from)?;
        }
        Ok(())
    }

    /// Rewrite `[[<directory>/from]]` wikilinks to `[[<directory>/to]]` in one page's
    /// `.md` file. The `]]` delimiter makes the match exact (a prefix path can't
    /// partial-match). No-op if the file is missing or unchanged.
    pub fn rewrite_wikilinks(
        &self,
        scope: &VaultScope,
        page_path: &str,
        from: &str,
        to: &str,
    ) -> Result<(), AppError> {
        let Some(content) = self.read_page(scope, page_path) else {
            return Ok(());
        };
        let rewritten = content.replace(
            &format!("[[{}]]", scope.vault_path(from)),
            &format!("[[{}]]", scope.vault_path(to)),
        );
        if rewritten != content {
            self.write_page(scope, page_path, &rewritten)?;
        }
        Ok(())
    }

    /// Whether a page's canonical `.md` file exists on disk - the cheap fast-path
    /// probe `reconcile_files` uses before doing any vault walk.
    pub fn page_exists(&self, scope: &VaultScope, path: &str) -> bool {
        Self::rel_md(path).is_ok_and(|rel| {
            self.memory_dir(scope)
                .map(|base| base.join(rel).exists())
                .unwrap_or(false)
        })
    }

    /// Fast BLAKE3 fingerprint of one canonical page file. The public sync revision
    /// remains SHA-256; this value is only for startup mirror comparison.
    pub(crate) fn page_fingerprint(
        &self,
        scope: &VaultScope,
        path: &str,
    ) -> Result<Option<[u8; 32]>, AppError> {
        let rel = Self::rel_md(path)?;
        let full = self.memory_dir(scope)?.join(rel);
        let file = match std::fs::File::open(&full) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "pkm storage: fingerprint {}: {error}",
                    full.display()
                )));
            }
        };
        let mut hasher = blake3::Hasher::new();
        hasher
            .update_reader(&mut BufReader::new(file))
            .map_err(|error| {
                AppError::Internal(format!(
                    "pkm storage: fingerprint {}: {error}",
                    full.display()
                ))
            })?;
        Ok(Some(*hasher.finalize().as_bytes()))
    }

    pub(crate) fn content_fingerprint(content: &str) -> [u8; 32] {
        *blake3::hash(content.as_bytes()).as_bytes()
    }

    /// Walk the Memory directory and return every `.md` file as `(page_path, uid)` -
    /// `page_path` is the file's location as a clean page path (directory + `.md`
    /// stripped), `uid` is the `uid:` frontmatter value we stamped (None for
    /// user-authored files we didn't mint). Used by `reconcile_files` to match
    /// files ↔ DB pages by identity. Only the Memory directory is walked (User Vault
    /// directories are the user's, never reconciled).
    pub fn list_page_files(&self, scope: &VaultScope) -> Vec<(String, Option<String>)> {
        let Ok(base) = self.memory_dir(scope) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in ignore::WalkBuilder::new(&base)
            .hidden(false)
            .build()
            .flatten()
        {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(rel) = p.strip_prefix(&base) else {
                continue;
            };
            let page_path = rel.to_string_lossy().trim_end_matches(".md").to_string();
            if page_path.is_empty() {
                continue;
            }
            let uid = std::fs::read_to_string(p).ok().and_then(|c| parse_uid(&c));
            out.push((page_path, uid));
        }
        out
    }

    /// Remove empty directories below the managed Memory root, deepest first.
    /// Non-empty directories (including those containing non-Markdown artifacts) stay.
    pub fn remove_empty_page_directories(&self, scope: &VaultScope) -> Result<(), AppError> {
        let base = self.memory_dir(scope)?;
        let mut directories = ignore::WalkBuilder::new(&base)
            .hidden(false)
            .build()
            .flatten()
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_dir()))
            .map(|entry| entry.into_path())
            .filter(|path| path != &base)
            .collect::<Vec<_>>();
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for directory in directories {
            match std::fs::remove_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => {
                    return Err(AppError::Internal(format!(
                        "pkm storage: remove empty directory {}: {error}",
                        directory.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{PkmStorage, VaultScope};
    use crate::core::Handle;
    use crate::storage::StorageService;

    fn test_storage() -> (PkmStorage, VaultScope, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_string_lossy().to_string();
        let config = crate::core::config::Config {
            storage: crate::core::config::StorageConfig {
                data_dir: base.clone(),
                shared_config_dir: format!("{base}/config"),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = PkmStorage::new(StorageService::new(&config));
        let handle = Handle::try_new("testuser").unwrap();
        let vault = storage.vault_scope(handle, "Memory").unwrap();
        (storage, vault, tmp)
    }

    #[test]
    fn rewrite_wikilinks_is_exact_and_leaves_siblings_alone() {
        let (storage, vault, _tmp) = test_storage();
        storage
            .write_page(&vault, "people/alice",
                "Uses [[Memory/services/postgres]], [[Memory/services/redis]], and [[Memory/services/postgres-2]].",
            )
            .unwrap();

        storage
            .rewrite_wikilinks(
                &vault,
                "people/alice",
                "services/postgres",
                "services/company-postgres",
            )
            .unwrap();

        let out = storage.read_page(&vault, "people/alice").unwrap();
        assert!(
            out.contains("[[Memory/services/company-postgres]]"),
            "renamed link updated"
        );
        assert!(
            !out.contains("[[Memory/services/postgres]]"),
            "old link gone"
        );
        assert!(
            out.contains("[[Memory/services/redis]]"),
            "unrelated link untouched"
        );
        assert!(
            out.contains("[[Memory/services/postgres-2]]"),
            "prefix-colliding link untouched"
        );
    }

    #[test]
    fn reset_deletes_only_the_managed_memory_directory() {
        let (storage, vault, _tmp) = test_storage();
        storage
            .write_page(&vault, "people/alice", "managed")
            .unwrap();
        storage
            .write_user_note(vault.handle(), "Work Notes/standup", "external")
            .unwrap();

        storage.delete_memory_directory(&vault).unwrap();

        assert!(storage.read_page(&vault, "people/alice").is_none());
        assert_eq!(
            std::fs::read_to_string(vault.root().join("Work Notes/standup.md")).unwrap(),
            "external"
        );
        storage.delete_memory_directory(&vault).unwrap();
    }

    #[test]
    fn page_vault_path_and_from_abs_round_trip() {
        let (_storage, vault, _tmp) = test_storage();
        assert_eq!(vault.vault_path("people/bob"), "Memory/people/bob");
        assert_eq!(
            vault.page_from_any("Memory/people/bob").as_deref(),
            Some("people/bob")
        );
        let abs = vault.root().join("Memory/people/bob.md");
        assert_eq!(
            vault.page_from_any(&abs.to_string_lossy()).as_deref(),
            Some("people/bob")
        );
        assert_eq!(vault.page_from_any("Work Notes/standup"), None);
    }

    /// `VaultScope::new` validates the directory, but storage re-checks it at the point
    /// of use - the write backstop. These scopes are built with `new_unchecked` precisely
    /// to prove the second check is real: a bad value that reached storage by some path
    /// that skipped validation still cannot `join` its way out of the user's root.
    #[test]
    fn bad_directory_segment_is_refused_by_storage() {
        let (storage, vault, _tmp) = test_storage();
        for bad in ["..", "../escape", "a/b", "  ", ""] {
            let bad_vault =
                VaultScope::new_unchecked(vault.handle().clone(), bad, vault.root().to_path_buf());
            assert!(
                storage
                    .write_page(&bad_vault, "people/bob", "body")
                    .is_err(),
                "write_page must refuse directory {bad:?}"
            );
            assert!(
                storage.read_page(&bad_vault, "people/bob").is_none(),
                "read_page must refuse directory {bad:?}"
            );
            assert!(
                !storage.page_exists(&bad_vault, "people/bob"),
                "page_exists must be false for directory {bad:?}"
            );
            assert!(
                storage.delete_page(&bad_vault, "people/bob").is_err(),
                "delete_page must refuse directory {bad:?}"
            );
            assert!(
                VaultScope::new(vault.handle().clone(), bad, vault.root().to_path_buf()).is_err(),
                "VaultScope::new must refuse directory {bad:?}"
            );
        }
        assert!(storage.write_page(&vault, "people/bob", "body").is_ok());
        assert!(PkmStorage::validate_directory("Brain").is_ok());
    }

    #[test]
    fn move_page_file_preserves_content_and_removes_old() {
        let (storage, vault, _tmp) = test_storage();
        storage
            .write_page(&vault, "services/postgres", "# Postgres\n\nbody")
            .unwrap();

        storage
            .move_page_file(&vault, "services/postgres", "services/company-postgres")
            .unwrap();

        assert!(
            storage.read_page(&vault, "services/postgres").is_none(),
            "old file gone"
        );
        assert_eq!(
            storage
                .read_page(&vault, "services/company-postgres")
                .as_deref(),
            Some("# Postgres\n\nbody"),
            "content moved intact"
        );
    }
}
