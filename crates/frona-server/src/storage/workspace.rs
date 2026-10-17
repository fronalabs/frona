use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::core::error::AppError;

use super::path::resolve_workspace_path;

/// Relative reads prefer earlier layers, then shared defaults. Absolute paths
/// select their actual layer; writes are confined to the first layer.
/// Checks resolve symlinks but do not prevent replacement before filesystem I/O.
/// `exists`, `resolve_path`, and `base_path` remain trusted-caller conveniences,
/// not authorization guards.
pub struct Workspace {
    layers: Vec<PathBuf>,
    shared_dir: Option<PathBuf>,
}

impl Workspace {
    pub fn new(layers: Vec<PathBuf>, shared_dir: Option<PathBuf>) -> Self {
        Self { layers, shared_dir }
    }

    pub fn read(&self, path: &str) -> Option<String> {
        for layer in self.layers.iter().chain(self.shared_dir.iter()) {
            let Ok(full) = resolve_workspace_path(layer, Path::new(path)) else {
                continue;
            };
            if let Ok(content) = std::fs::read_to_string(&full) {
                return Some(content);
            }
        }
        None
    }

    pub fn write(&self, path: &str, content: &str) -> Result<(), AppError> {
        self.write_bytes(path, content.as_bytes())
    }

    pub fn write_bytes(&self, path: &str, content: &[u8]) -> Result<(), AppError> {
        let full = resolve_workspace_path(&self.layers[0], Path::new(path))?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AppError::Internal(format!("Failed to create directories: {e}")))?;
        }
        std::fs::write(&full, content)
            .map_err(|e| AppError::Internal(format!("Failed to write {}: {e}", full.display())))
    }

    pub fn exists(&self, path: &str) -> bool {
        for layer in &self.layers {
            if layer.join(path).exists() {
                return true;
            }
        }

        self.shared_dir
            .as_ref()
            .is_some_and(|shared| shared.join(path).exists())
    }

    pub fn read_dir(&self, path: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        for layer in self.layers.iter().chain(self.shared_dir.iter()) {
            let Ok(dir) = resolve_workspace_path(layer, Path::new(path)) else {
                continue;
            };
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    seen.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
        let mut result: Vec<String> = seen.into_iter().collect();
        result.sort();
        result
    }

    pub fn resolve_path(&self, path: &str) -> Option<PathBuf> {
        for layer in &self.layers {
            let full = layer.join(path);
            if full.exists() {
                return Some(full);
            }
        }
        None
    }

    pub fn base_path(&self) -> &Path {
        &self.layers[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn contained_paths_preserve_layers_and_primary_only_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let layers: Vec<_> = ["first", "second", "shared"]
            .iter()
            .map(|name| tmp.path().join(name))
            .collect();
        for (index, layer) in layers.iter().enumerate() {
            std::fs::create_dir_all(layer.join("reports")).unwrap();
            std::fs::write(layer.join("file.txt"), index.to_string()).unwrap();
            std::fs::write(layer.join(format!("only-{index}.txt")), index.to_string()).unwrap();
        }
        let ws = Workspace::new(layers[..2].to_vec(), Some(layers[2].clone()));
        assert_eq!(ws.read("reports/../file.txt").as_deref(), Some("0"));
        assert_eq!(ws.read("only-1.txt").as_deref(), Some("1"));
        assert_eq!(ws.read("only-2.txt").as_deref(), Some("2"));
        assert_eq!(ws.read("missing.txt"), None);
        assert!(ws.read_dir("missing").is_empty());
        assert_eq!(
            ws.read_dir("reports/.."),
            vec![
                "file.txt",
                "only-0.txt",
                "only-1.txt",
                "only-2.txt",
                "reports"
            ]
        );
        for (index, layer) in layers.iter().enumerate() {
            assert_eq!(
                ws.read(layer.join("file.txt").to_str().unwrap()),
                Some(index.to_string())
            );
            assert_eq!(
                ws.read_dir(layer.to_str().unwrap()),
                vec![
                    "file.txt".to_string(),
                    format!("only-{index}.txt"),
                    "reports".to_string()
                ]
            );
        }
        for path in [
            "reports/../nested/report..txt",
            "new/../file.txt",
            ".../data.txt",
            ".config/settings",
        ] {
            ws.write(path, "new").unwrap();
            assert_eq!(ws.read(path).as_deref(), Some("new"));
        }
        let absolute = layers[0].join("absolute/new.txt");
        ws.write_bytes(absolute.to_str().unwrap(), b"absolute")
            .unwrap();
        assert_eq!(
            ws.read(absolute.to_str().unwrap()).as_deref(),
            Some("absolute")
        );
        for layer in &layers[1..] {
            assert!(!layer.join("nested").exists());
            assert!(
                ws.write(layer.join("file.txt").to_str().unwrap(), "denied")
                    .is_err()
            );
        }
        assert_eq!(
            std::fs::read_to_string(layers[1].join("file.txt")).unwrap(),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(layers[2].join("file.txt")).unwrap(),
            "2"
        );
    }

    #[test]
    fn escapes_and_outward_links_have_no_write_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        let shared = tmp.path().join("shared");
        let outside = tmp.path().join("workspace-other");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        for layer in [&root, &shared] {
            std::fs::create_dir_all(layer.join("nested")).unwrap();
            symlink(&outside, layer.join("outward")).unwrap();
            symlink(outside.join("missing"), layer.join("dangling")).unwrap();
        }
        let ws = Workspace::new(vec![root.clone()], Some(shared));
        for directory in [
            outside.to_str().unwrap(),
            "../workspace-other",
            "nested/../../workspace-other",
            "outward",
            "dangling",
        ] {
            assert!(ws.read_dir(directory).is_empty(), "{directory}");
            assert_eq!(
                ws.read(&format!("{directory}/secret.txt")),
                None,
                "{directory}"
            );
            assert!(
                ws.write(&format!("{directory}/secret.txt"), "overwrite")
                    .is_err(),
                "{directory}"
            );
            assert!(
                ws.write_bytes(&format!("{directory}/new/file.txt"), b"new")
                    .is_err(),
                "{directory}"
            );
        }
        assert!(!outside.join("new").exists());
        assert!(!outside.join("missing").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
            "secret"
        );
        assert!(ws.write(".", "root").is_err());
        assert!(root.is_dir());
    }

    #[test]
    fn inward_link_parent_resolves_before_read_write_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(root.join("actual/deep")).unwrap();
        std::fs::write(root.join("file.txt"), "lexical target").unwrap();
        std::fs::write(root.join("actual/file.txt"), "symlink target").unwrap();
        symlink(root.join("actual/deep"), root.join("link")).unwrap();
        let ws = Workspace::new(vec![root.clone()], None);
        assert_eq!(
            ws.read("link/../file.txt").as_deref(),
            Some("symlink target")
        );
        assert_eq!(ws.read_dir("link/.."), vec!["deep", "file.txt"]);
        ws.write("link/../new/nested.txt", "new").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("actual/new/nested.txt")).unwrap(),
            "new"
        );
        assert!(!root.join("new").exists());
    }

    #[test]
    fn new_files_can_create_missing_workspace_and_normalized_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("missing/workspace");
        let ws = Workspace::new(vec![root.clone()], None);
        ws.write("new/deep/../report.txt", "new").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("new/report.txt")).unwrap(),
            "new"
        );
        assert!(!root.join("new/deep").exists());
        assert!(ws.write("new/../../../outside.txt", "denied").is_err());
        assert!(!tmp.path().join("outside.txt").exists());
    }
}
