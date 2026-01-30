use super::defaults::get_embedded_default;
use super::source::{AgentConfigSource, ConfigEntry};

#[derive(Clone)]
pub struct AgentConfigResolver {
    config_source: AgentConfigSource,
}

impl AgentConfigResolver {
    pub fn new(config_source: AgentConfigSource) -> Self {
        Self { config_source }
    }

    pub fn resolve_fs(&self, agent_id: &str, category: &str, name: &str) -> Option<ConfigEntry> {
        if let Some(entry) = self.config_source.get(agent_id, category, name) {
            return Some(entry.clone());
        }

        if agent_id != "system"
            && let Some(entry) = self.config_source.get("system", category, name)
        {
            return Some(entry.clone());
        }

        get_embedded_default(agent_id, category, name)
    }
}
