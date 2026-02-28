use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Clone)]
pub struct PromptLoader {
    base_dir: PathBuf,
}

impl PromptLoader {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    pub fn read(&self, name: &str) -> Option<String> {
        let path = self.base_dir.join(name);
        std::fs::read_to_string(&path).ok()
    }

    pub fn list_dir(&self, dir: &str) -> Vec<String> {
        let mut paths = BTreeSet::new();

        let full_dir = self.base_dir.join(dir);
        if let Ok(entries) = std::fs::read_dir(&full_dir) {
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

    fn shared_prompts_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("resources")
            .join("prompts")
    }

    #[test]
    fn reads_prompt_from_base_dir() {
        let loader = PromptLoader::new(shared_prompts_dir());
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
    fn list_dir_returns_files() {
        let loader = PromptLoader::new(shared_prompts_dir());
        let files = loader.list_dir("tools");
        assert!(!files.is_empty(), "Expected tool files in dir");
        assert!(files.iter().any(|f| f.ends_with("shell.md")));
        assert!(files.iter().any(|f| f.ends_with("python.md")));
    }
}
