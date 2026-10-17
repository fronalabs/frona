use std::fs::File;
use std::path::{Component, Path};

use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags, open, openat};

use crate::core::error::AppError;

use super::{validate_no_traversal, validate_relative_path};

/// Resolve allowed symlinks, then open through directory handles without following
/// any replacement symlinks. Callers must authorize the workspace before calling.
/// The returned handle pins the opened file even if its pathname is later changed.
pub fn open_workspace_file(workspace: &Path, relative: &str) -> Result<File, AppError> {
    validate_relative_path(relative)?;
    let workspace = std::fs::canonicalize(workspace).map_err(download_error)?;
    let root = open(
        &workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| download_error(error.into()))?;
    let resolved = std::fs::canonicalize(workspace.join(relative)).map_err(download_error)?;
    validate_no_traversal(&resolved, &workspace.to_string_lossy())?;
    let relative = resolved
        .strip_prefix(&workspace)
        .map_err(|_| AppError::Validation("Path escapes allowed directory".into()))?;
    open_relative_file(root, relative)
}

fn open_relative_file(mut directory: OwnedFd, relative: &Path) -> Result<File, AppError> {
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(AppError::Validation("Path must be relative".into()));
        };
        let last = components.peek().is_none();
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if !last {
            flags |= OFlags::DIRECTORY;
        }
        // Every component is opened relative to a pinned directory. A renamed
        // directory remains that same capability; replacement links are never followed.
        let opened = openat(&directory, name, flags, Mode::empty())
            .map_err(|error| download_error(error.into()))?;
        if last {
            return Ok(File::from(opened));
        }
        directory = opened;
    }
    Err(AppError::Validation("File path is required".into()))
}

fn download_error(error: std::io::Error) -> AppError {
    match error.raw_os_error() {
        Some(libc::ENOENT) => AppError::NotFound("File not found".into()),
        Some(libc::ELOOP | libc::ENOTDIR) => {
            AppError::Validation("Download path changed or is not a regular file path".into())
        }
        _ => AppError::Internal(format!("Failed to open download: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;

    fn open_root(path: &Path) -> OwnedFd {
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    #[test]
    fn rejects_leaf_symlink_substitution_after_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let outside = tmp.path().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let target = workspace.join("file.txt");
        std::fs::write(&target, "allowed").unwrap();
        let root = open_root(&workspace);
        let resolved = std::fs::canonicalize(&target).unwrap();
        let workspace = std::fs::canonicalize(&workspace).unwrap();
        let relative = resolved.strip_prefix(&workspace).unwrap();

        std::fs::remove_file(&target).unwrap();
        symlink(&outside, &target).unwrap();
        assert!(matches!(
            open_relative_file(root, relative),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn rejects_directory_symlink_substitution_after_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("nested")).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file.txt"), "secret").unwrap();
        let target = workspace.join("nested/file.txt");
        std::fs::write(&target, "allowed").unwrap();
        let root = open_root(&workspace);
        let resolved = std::fs::canonicalize(&target).unwrap();
        let workspace = std::fs::canonicalize(&workspace).unwrap();
        let relative = resolved.strip_prefix(&workspace).unwrap();

        std::fs::rename(workspace.join("nested"), workspace.join("old-nested")).unwrap();
        symlink(&outside, workspace.join("nested")).unwrap();
        assert!(matches!(
            open_relative_file(root, relative),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn opened_handle_retains_original_file_after_path_substitution() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let target = workspace.join("file.txt");
        std::fs::write(&target, "allowed").unwrap();
        let outside = tmp.path().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let mut file = open_workspace_file(&workspace, "file.txt").unwrap();

        std::fs::remove_file(&target).unwrap();
        symlink(&outside, &target).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "allowed");
    }
}
