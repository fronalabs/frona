use std::collections::HashSet;
use std::path::PathBuf;

use crate::core::error::AppError;
use super::prompt::PromptLoader;

#[derive(Clone)]
pub struct AgentWorkspaceManager {
    workspace_base: PathBuf,
    shared_agents_dir: PathBuf,
}

impl AgentWorkspaceManager {
    pub fn new(workspace_base: impl Into<PathBuf>, shared_agents_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspace_base: workspace_base.into(),
            shared_agents_dir: shared_agents_dir.into(),
        }
    }

    pub fn get(&self, agent_id: &str) -> AgentWorkspace {
        let sanitized = agent_id.replace(['/', '\\', ':', '\0'], "_");
        let workspace_path = self.workspace_base.join(&sanitized);
        let shared_path = self.shared_agents_dir.join(&sanitized);

        AgentWorkspace {
            layers: vec![workspace_path],
            shared_dir: shared_path,
        }
    }

    pub fn builtin_agent_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.shared_agents_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && let Some(name) = entry.file_name().to_str()
                {
                    ids.push(name.to_string());
                }
            }
        }
        ids.sort();
        ids
    }
}

pub struct AgentWorkspace {
    layers: Vec<PathBuf>,
    shared_dir: PathBuf,
}

impl AgentWorkspace {
    pub fn read(&self, path: &str) -> Option<String> {
        for layer in &self.layers {
            let full = layer.join(path);
            if let Ok(content) = std::fs::read_to_string(&full) {
                return Some(content);
            }
        }

        let shared_path = self.shared_dir.join(path);
        std::fs::read_to_string(&shared_path).ok()
    }

    pub fn write(&self, path: &str, content: &str) -> Result<(), AppError> {
        let full = self.layers[0].join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AppError::Internal(format!("Failed to create directories: {e}"))
            })?;
        }
        std::fs::write(&full, content).map_err(|e| {
            AppError::Internal(format!("Failed to write {}: {e}", full.display()))
        })
    }

    pub fn exists(&self, path: &str) -> bool {
        for layer in &self.layers {
            if layer.join(path).exists() {
                return true;
            }
        }

        self.shared_dir.join(path).exists()
    }

    pub fn read_dir(&self, path: &str) -> Vec<String> {
        let mut seen = HashSet::new();

        for layer in &self.layers {
            let dir = layer.join(path);
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    seen.insert(name);
                }
            }
        }

        let shared_path = self.shared_dir.join(path);
        if let Ok(entries) = std::fs::read_dir(&shared_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                seen.insert(name);
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
}

pub struct AgentPromptLoader<'a> {
    workspace: &'a AgentWorkspace,
    global: &'a PromptLoader,
}

impl<'a> AgentPromptLoader<'a> {
    pub fn new(workspace: &'a AgentWorkspace, global: &'a PromptLoader) -> Self {
        Self { workspace, global }
    }

    pub fn read(&self, name: &str) -> Option<String> {
        if let Some(content) = self.workspace.read(name) {
            return Some(content);
        }
        let prompts_path = format!("prompts/{name}");
        if let Some(content) = self.workspace.read(&prompts_path) {
            return Some(content);
        }
        self.global.read(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("resources")
    }

    fn test_manager(tmp: &std::path::Path) -> AgentWorkspaceManager {
        AgentWorkspaceManager::new(tmp.join("workspaces"), shared_dir().join("agents"))
    }

    #[test]
    fn test_read_builtin() {
        let tmp = std::env::temp_dir().join("frona_ws_test_read_builtin");
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");
        let content = ws.read("AGENT.md");
        assert!(content.is_some(), "Should read AGENT.md from shared layer");
        assert!(content.unwrap().contains("You're not a chatbot"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_read_not_found() {
        let tmp = std::env::temp_dir().join("frona_ws_test_read_not_found");
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");
        assert!(ws.read("nonexistent.md").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_read_data_layer_shadows_builtin() {
        let tmp = std::env::temp_dir().join("frona_ws_test_shadow");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");

        ws.write("AGENT.md", "Custom prompt").unwrap();
        let content = ws.read("AGENT.md").unwrap();
        assert_eq!(content, "Custom prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_creates_file() {
        let tmp = std::env::temp_dir().join("frona_ws_test_write");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("test_agent");

        ws.write("test.md", "hello").unwrap();
        assert_eq!(ws.read("test.md").unwrap(), "hello");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_creates_parent_dirs() {
        let tmp = std::env::temp_dir().join("frona_ws_test_write_nested");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("test_agent");

        ws.write("nested/dir/file.md", "deep content").unwrap();
        assert_eq!(ws.read("nested/dir/file.md").unwrap(), "deep content");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_exists_builtin() {
        let tmp = std::env::temp_dir().join("frona_ws_test_exists_builtin");
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");
        assert!(ws.exists("AGENT.md"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_exists_missing() {
        let tmp = std::env::temp_dir().join("frona_ws_test_exists_missing");
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");
        assert!(!ws.exists("nonexistent.md"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_read_dir_builtin() {
        let tmp = std::env::temp_dir().join("frona_ws_test_read_dir_builtin");
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");
        let entries = ws.read_dir("");
        assert!(entries.contains(&"AGENT.md".to_string()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_read_dir_merged() {
        let tmp = std::env::temp_dir().join("frona_ws_test_read_dir_merged");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");

        ws.write("CUSTOM.md", "custom").unwrap();
        let entries = ws.read_dir("");
        assert!(entries.contains(&"AGENT.md".to_string()), "Should contain shared AGENT.md");
        assert!(entries.contains(&"CUSTOM.md".to_string()), "Should contain data layer CUSTOM.md");
        let unique_count = entries.len();
        let deduped: std::collections::HashSet<_> = entries.iter().collect();
        assert_eq!(unique_count, deduped.len(), "Should be deduplicated");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_resolve_path_filesystem() {
        let tmp = std::env::temp_dir().join("frona_ws_test_resolve_path_fs");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("test_agent");

        ws.write("skills/test/SKILL.md", "skill content").unwrap();
        let resolved = ws.resolve_path("skills/test/SKILL.md");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_resolve_path_builtin_returns_none() {
        let tmp = std::env::temp_dir().join("frona_ws_test_resolve_path_builtin");
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");
        let resolved = ws.resolve_path("AGENT.md");
        assert!(resolved.is_none(), "resolve_path should not return shared files");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_builtin_agent_ids() {
        let tmp = std::env::temp_dir().join("frona_ws_test_builtin_ids");
        let mgr = test_manager(&tmp);
        let ids = mgr.builtin_agent_ids();
        assert!(ids.contains(&"system".to_string()), "Should include 'system' agent");
        assert!(ids.contains(&"researcher".to_string()), "Should include 'researcher' agent");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_prompt_loader_returns_agent_specific() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_agent");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("system");

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("TITLE.md");
        assert!(content.is_some(), "Should read TITLE.md from agent workspace");
        assert!(content.unwrap().contains("title generator"));
    }

    #[test]
    fn test_prompt_loader_falls_back_to_global() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_fallback");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("nonexistent_agent");

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("CHAT_COMPACTION.md");
        assert!(content.is_some(), "Should fall back to global prompt");
        assert!(content.unwrap().contains("conversation summarizer"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_prompt_loader_agent_shadows_global() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_shadow");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("test_agent");

        ws.write("prompts/CHAT_COMPACTION.md", "Agent-specific compaction").unwrap();

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("CHAT_COMPACTION.md").unwrap();
        assert_eq!(content, "Agent-specific compaction");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_prompt_loader_reads_from_agent_root() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_root");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("test_agent");

        ws.write("TOOLS.md", "Root-level prompt").unwrap();

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("TOOLS.md");
        assert!(content.is_some(), "Should read TOOLS.md from agent workspace root");
        assert_eq!(content.unwrap(), "Root-level prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_prompt_loader_root_shadows_prompts_dir() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_root_shadow");
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = test_manager(&tmp);
        let ws = mgr.get("test_agent");

        ws.write("MY_PROMPT.md", "From root").unwrap();
        ws.write("prompts/MY_PROMPT.md", "From prompts dir").unwrap();

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("MY_PROMPT.md").unwrap();
        assert_eq!(content, "From root", "Root should shadow prompts/ dir");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
