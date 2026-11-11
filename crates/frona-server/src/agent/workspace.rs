use crate::storage::Workspace;
use frona_text::prompt::PromptLoader;

pub struct AgentPromptLoader<'a> {
    workspace: &'a Workspace,
    global: &'a PromptLoader,
}

impl<'a> AgentPromptLoader<'a> {
    pub fn new(workspace: &'a Workspace, global: &'a PromptLoader) -> Self {
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
    use std::path::PathBuf;

    use super::*;
    use crate::core::config::Config;
    use crate::storage::StorageService;

    fn shared_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("resources")
    }

    fn test_service(tmp: &std::path::Path) -> StorageService {
        let mut config = Config::default();
        config.storage.data_dir = tmp.to_string_lossy().into_owned();
        config.storage.shared_config_dir = shared_dir().to_string_lossy().into_owned();
        StorageService::new(&config)
    }

    #[test]
    fn test_prompt_loader_returns_agent_specific() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_agent");
        let _ = std::fs::remove_dir_all(&tmp);
        let svc = test_service(&tmp);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("system"));

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("TITLE.md");
        assert!(
            content.is_some(),
            "Should read TITLE.md from agent workspace"
        );
        assert!(content.unwrap().contains("title generator"));
    }

    #[test]
    fn test_prompt_loader_falls_back_to_global() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_fallback");
        let _ = std::fs::remove_dir_all(&tmp);
        let svc = test_service(&tmp);
        let ws = svc.agent_workspace(
            &crate::handle!("test-user"),
            &crate::handle!("nonexistent_agent"),
        );

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
        let svc = test_service(&tmp);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("test_agent"));

        ws.write("prompts/CHAT_COMPACTION.md", "Agent-specific compaction")
            .unwrap();

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
        let svc = test_service(&tmp);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("test_agent"));

        ws.write("TOOLS.md", "Root-level prompt").unwrap();

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("TOOLS.md");
        assert!(
            content.is_some(),
            "Should read TOOLS.md from agent workspace root"
        );
        assert_eq!(content.unwrap(), "Root-level prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_prompt_loader_root_shadows_prompts_dir() {
        let tmp = std::env::temp_dir().join("frona_ws_test_prompt_root_shadow");
        let _ = std::fs::remove_dir_all(&tmp);
        let svc = test_service(&tmp);
        let ws = svc.agent_workspace(&crate::handle!("test-user"), &crate::handle!("test_agent"));

        ws.write("MY_PROMPT.md", "From root").unwrap();
        ws.write("prompts/MY_PROMPT.md", "From prompts dir")
            .unwrap();

        let global = PromptLoader::new(shared_dir().join("prompts"));
        let loader = AgentPromptLoader::new(&ws, &global);
        let content = loader.read("MY_PROMPT.md").unwrap();
        assert_eq!(content, "From root", "Root should shadow prompts/ dir");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
