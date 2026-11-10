use super::FileError;
use std::path::{Component, Path, PathBuf};

/// Resolve a filesystem destination, including the missing tail of a new file.
/// Existing symlinks are resolved before `..`; only missing components are
/// normalized lexically. Callers must authorize the returned absolute path.
/// This does not pin the path against replacement between checking and I/O.
pub fn resolve_filesystem_path(path: &Path) -> Result<PathBuf, FileError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(filesystem_path_error)?
            .join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                match std::fs::metadata(&resolved) {
                    Ok(metadata) if !metadata.is_dir() => {
                        return Err(FileError::Validation(
                            "Path component is not a directory".into(),
                        ));
                    }
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(filesystem_path_error(error));
                    }
                    _ => {}
                }
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        // Fail on dangling symlinks rather than treating them as
                        // ordinary missing filenames that could redirect writes.
                        resolved =
                            std::fs::canonicalize(&resolved).map_err(filesystem_path_error)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(filesystem_path_error(error)),
                }
            }
        }
    }
    Ok(resolved)
}

/// Resolve a path against a workspace and require the final destination to be
/// inside it. Equality is permitted for directory reads; writers retain their
/// normal filesystem errors for directory targets. This is a check, not a lock.
pub fn resolve_workspace_path(root: &Path, path: &Path) -> Result<PathBuf, FileError> {
    let root = resolve_filesystem_path(root)?;
    let resolved = resolve_filesystem_path(&root.join(path))?;
    if !resolved.starts_with(&root) {
        return Err(FileError::Validation(
            "Path escapes allowed directory".into(),
        ));
    }
    Ok(resolved)
}

fn filesystem_path_error(error: std::io::Error) -> FileError {
    match error.kind() {
        std::io::ErrorKind::NotFound => FileError::NotFound("Path not found".into()),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotADirectory => {
            FileError::Validation(format!("Invalid filesystem path: {error}"))
        }
        _ => FileError::Io(format!("Failed to resolve filesystem path: {error}")),
    }
}
