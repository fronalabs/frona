use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use crate::agent::workspace::AgentPromptLoader;
use crate::core::Handle;
use crate::core::template::render_template;
use crate::storage::StorageService;

#[derive(Clone)]
pub struct PromptLoader {
    base_dir: PathBuf,
    defaults: HashMap<String, String>,
}

impl PromptLoader {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            defaults: HashMap::new(),
        }
    }

    pub fn with_var(mut self, key: &str, value: &str) -> Self {
        self.defaults.insert(key.to_lowercase(), value.to_string());
        self
    }

    pub fn defaults(&self) -> &HashMap<String, String> {
        &self.defaults
    }

    pub fn read(&self, name: &str) -> Option<String> {
        self.read_with_vars(name, &[])
    }

    /// Optional files may be absent, but unreadable or invalid files are errors.
    pub fn try_read(&self, name: &str) -> Result<Option<String>, crate::core::error::AppError> {
        self.try_read_with_vars(name, &[])
    }

    pub fn try_read_with_vars(
        &self,
        name: &str,
        vars: &[(&str, &str)],
    ) -> Result<Option<String>, crate::core::error::AppError> {
        let raw = match std::fs::read_to_string(self.base_dir.join(name)) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(crate::core::error::AppError::Internal(format!(
                    "Read prompt {name}: {e}"
                )));
            }
        };
        let merged = self.merge_vars(vars);
        let refs: Vec<_> = merged
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        render_template(&raw, &refs).map(Some).map_err(|e| {
            crate::core::error::AppError::Internal(format!("Render prompt {name}: {e}"))
        })
    }

    pub fn read_with_vars(&self, name: &str, vars: &[(&str, &str)]) -> Option<String> {
        let path = self.base_dir.join(name);
        let raw = std::fs::read_to_string(&path).ok()?;
        let merged = self.merge_vars(vars);
        let merged_refs: Vec<(&str, &str)> = merged
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        render_template(&raw, &merged_refs).ok()
    }

    pub fn read_raw(&self, name: &str) -> Option<String> {
        let path = self.base_dir.join(name);
        std::fs::read_to_string(&path).ok()
    }

    fn merge_vars(&self, caller_vars: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut merged: HashMap<String, String> = self.defaults.clone();
        for (k, v) in caller_vars {
            merged.insert(k.to_lowercase(), v.to_string());
        }
        merged.into_iter().collect()
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

pub fn append_tagged_section(
    result: &mut String,
    tag: &str,
    header: Option<&str>,
    items: &[(String, String)],
) {
    if items.is_empty() {
        return;
    }
    result.push_str(&format!("\n\n<{tag}>\n"));
    if let Some(h) = header {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            result.push_str(trimmed);
            result.push('\n');
        }
    }
    for (key, value) in items {
        result.push_str(&format!("- {key}: {value}\n"));
    }
    result.push_str(&format!("</{tag}>"));
}

/// Renders standing instructions and skill expansions.
/// Callers persist dynamic text once; replay never renders it again.
#[derive(Clone)]
pub struct AgentPromptBuilder {
    prompts: PromptLoader,
    storage: StorageService,
    memory: std::sync::Arc<dyn crate::memory::service::MemoryService>,
    skills: crate::agent::skill::service::SkillService,
}

impl AgentPromptBuilder {
    pub fn new(
        prompts: PromptLoader,
        storage: StorageService,
        memory: std::sync::Arc<dyn crate::memory::service::MemoryService>,
        skills: crate::agent::skill::service::SkillService,
    ) -> Self {
        Self {
            prompts,
            storage,
            memory,
            skills,
        }
    }

    pub async fn render_skill(
        &self,
        user: &crate::auth::models::User,
        agent: &crate::agent::models::Agent,
        name: &str,
        prompt: &str,
    ) -> Result<Option<String>, crate::core::error::AppError> {
        let skills = self
            .skills
            .try_list(&user.handle, &agent.handle, agent.skills.as_deref())
            .await?;
        crate::chat::command::render::try_render_skill(name, prompt, &skills)
    }

    pub async fn build(
        &self,
        base_prompt: &str,
        identity: &BTreeMap<String, String>,
        agent_handle: &Handle,
        user: &crate::auth::User,
        additional_instructions: &[&str],
    ) -> Result<String, crate::core::error::AppError> {
        let mut result = base_prompt.to_string();
        if !["name", "creature", "vibe"]
            .iter()
            .all(|key| identity.keys().any(|k| k.eq_ignore_ascii_case(key)))
        {
            let ws = self.storage.agent_workspace(&user.handle, agent_handle);
            if let Some(text) = AgentPromptLoader::new(&ws, &self.prompts).read("IDENTITY.md") {
                result.push_str("\n\n");
                result.push_str(&text);
            }
        }
        for name in ["WORKSPACE.md", "TOOLS.md", "SKILLS.md", "SCHEDULING.md"] {
            if let Some(text) = self.prompts.read(name) {
                result.push_str("\n\n");
                result.push_str(&text);
            }
        }
        append_tagged_section(
            &mut result,
            "agent_identity",
            None,
            &identity
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>(),
        );
        // The wrapper is stable even when the first server is installed.
        if let Some(instructions) = self.prompts.read("MCP.md") {
            result.push_str("\n\n");
            result.push_str(&instructions);
        }

        for text in additional_instructions {
            result.push_str("\n\n");
            result.push_str(text);
        }
        let memory = self.memory.instructions(user).await?;
        if !memory.is_empty() {
            result.push_str("\n\n");
            result.push_str(&memory);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builder_keeps_instruction_order_and_scopes_memory_without_collecting_it() {
        use crate::agent::context::{AgentContextScope, AgentContextSection};
        use crate::core::error::AppError;
        use crate::memory::service::MemoryService;
        use std::sync::Arc;

        struct MemoryInstructions;
        #[async_trait::async_trait]
        impl MemoryService for MemoryInstructions {
            fn tools(&self) -> Vec<Arc<dyn crate::tool::AgentTool>> {
                vec![]
            }
            async fn instructions(&self, user: &crate::auth::User) -> Result<String, AppError> {
                if user.id == "unavailable" {
                    return Err(AppError::Internal("memory instructions unavailable".into()));
                }
                Ok(format!("Memory instructions for {}", user.id))
            }
            async fn collect(
                &self,
                _: &AgentContextScope<'_>,
            ) -> Result<Vec<AgentContextSection>, AppError> {
                panic!("Standing prompts must not collect dynamic memory");
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("WORKSPACE.md"), "Workspace instructions").unwrap();
        let storage = StorageService::new(&crate::core::config::Config::default());
        let builder = AgentPromptBuilder::new(
            PromptLoader::new(dir.path()),
            storage.clone(),
            Arc::new(MemoryInstructions),
            crate::agent::skill::service::SkillService::new(
                crate::agent::skill::registry::SkillRegistryClient::new(
                    reqwest::Client::new(),
                    dir.path(),
                ),
                crate::agent::skill::resolver::SkillResolver::new(
                    dir.path(),
                    storage.clone(),
                    dir.path(),
                ),
                storage,
                dir.path(),
                &crate::core::config::CacheConfig::default(),
            ),
        );
        let identity = BTreeMap::from([
            ("name".into(), "Agent".into()),
            ("creature".into(), "Assistant".into()),
            ("vibe".into(), "Helpful".into()),
        ]);
        let handle = Handle::try_new("agent").unwrap();
        let mut user = crate::auth::User {
            id: "alice".into(),
            handle: crate::handle!("alice"),
            email: "alice@example.com".into(),
            name: "Alice".into(),
            password_hash: String::new(),
            timezone: None,
            groups: vec![],
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let first = builder
            .build(
                "Base instructions",
                &identity,
                &handle,
                &user,
                &["Task instructions", "Conversation instructions"],
            )
            .await
            .unwrap();
        assert!(first.starts_with("Base instructions\n\nWorkspace instructions"));
        assert!(first.ends_with(
            "\n\nTask instructions\n\nConversation instructions\n\nMemory instructions for alice"
        ));
        assert_eq!(first.matches("Memory instructions").count(), 1);
        user.id = "bob".into();
        let second = builder
            .build("Base instructions", &identity, &handle, &user, &[])
            .await
            .unwrap();
        assert!(second.ends_with("Memory instructions for bob"));
        assert!(!second.contains("alice"));
        assert!(!second.contains("Task instructions"));
        user.id = "unavailable".into();
        assert!(
            builder
                .build("Base instructions", &identity, &handle, &user, &[])
                .await
                .is_err()
        );
    }

    fn shared_prompts_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
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

    #[test]
    fn read_renders_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.md"), "Hello {{name}}!").unwrap();
        let loader = PromptLoader::new(dir.path()).with_var("name", "World");
        let content = loader.read("test.md").unwrap();
        assert_eq!(content, "Hello World!");
    }

    #[test]
    fn read_with_vars_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.md"), "Hello {{name}}!").unwrap();
        let loader = PromptLoader::new(dir.path()).with_var("name", "Default");
        let content = loader
            .read_with_vars("test.md", &[("name", "Override")])
            .unwrap();
        assert_eq!(content, "Hello Override!");
    }

    #[test]
    fn read_with_vars_renders_active_call_template() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("active_call.md"),
            "[CALL_CONNECTED: Now speaking with {{caller_name}} ({{phone_number}}). Goal: {{objective}}.]",
        ).unwrap();
        let loader = PromptLoader::new(dir.path());
        let content = loader
            .read_with_vars(
                "active_call.md",
                &[
                    ("caller_name", "Alice"),
                    ("phone_number", "+1234567890"),
                    ("objective", "Schedule meeting"),
                ],
            )
            .unwrap();
        assert_eq!(
            content,
            "[CALL_CONNECTED: Now speaking with Alice (+1234567890). Goal: Schedule meeting.]"
        );
    }
}
