use std::collections::BTreeSet;
use std::path::PathBuf;

use include_dir::{Dir, include_dir};

static BUILTIN_PROMPTS: Dir = include_dir!("$CARGO_MANIFEST_DIR/config/prompts");

#[derive(Clone)]
pub struct PromptLoader {
    override_dir: PathBuf,
}

impl PromptLoader {
    pub fn new(override_dir: impl Into<PathBuf>) -> Self {
        Self {
            override_dir: override_dir.into(),
        }
    }

    pub fn read(&self, name: &str) -> Option<String> {
        let override_path = self.override_dir.join(name);
        if let Ok(content) = std::fs::read_to_string(&override_path) {
            return Some(content);
        }

        BUILTIN_PROMPTS
            .get_file(name)
            .and_then(|f| f.contents_utf8())
            .map(|s| s.to_string())
    }

    pub fn list_dir(&self, dir: &str) -> Vec<String> {
        let mut paths = BTreeSet::new();

        if let Some(builtin_dir) = BUILTIN_PROMPTS.get_dir(dir) {
            for file in builtin_dir.files() {
                if let Some(path) = file.path().to_str() {
                    paths.insert(path.to_string());
                }
            }
        }

        let override_dir = self.override_dir.join(dir);
        if let Ok(entries) = std::fs::read_dir(&override_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                    && let Some(name) = entry.file_name().to_str()
                {
                    paths.insert(format!("{dir}/{name}"));
                }
            }
        }

        paths.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_builtin_prompt() {
        let loader = PromptLoader::new("/nonexistent");
        let content = loader.read("CHAT_COMPACTION.md");
        assert!(content.is_some());
        assert!(content.unwrap().contains("conversation summarizer"));
    }

    #[test]
    fn returns_none_for_missing_prompt() {
        let loader = PromptLoader::new("/nonexistent");
        assert!(loader.read("DOES_NOT_EXIST.md").is_none());
    }

    #[test]
    fn list_dir_returns_builtin_files() {
        let loader = PromptLoader::new("/nonexistent");
        let files = loader.list_dir("tools");
        assert!(!files.is_empty(), "Expected tool files in builtin dir");
        assert!(files.iter().any(|f| f.ends_with("shell.md")));
        assert!(files.iter().any(|f| f.ends_with("python.md")));
    }

    #[test]
    fn list_dir_merges_override_files() {
        let tmp = std::env::temp_dir().join("frona_prompt_list_dir_test");
        let _ = std::fs::remove_dir_all(&tmp);
        let tools_dir = tmp.join("tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
        std::fs::write(tools_dir.join("custom_tool.md"), "custom").unwrap();

        let loader = PromptLoader::new(&tmp);
        let files = loader.list_dir("tools");
        assert!(files.iter().any(|f| f == "tools/custom_tool.md"));
        assert!(files.iter().any(|f| f.ends_with("shell.md")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn filesystem_override_shadows_builtin() {
        let tmp = std::env::temp_dir().join("frona_prompt_loader_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("CHAT_COMPACTION.md"), "Custom prompt").unwrap();

        let loader = PromptLoader::new(&tmp);
        let content = loader.read("CHAT_COMPACTION.md").unwrap();
        assert_eq!(content, "Custom prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
