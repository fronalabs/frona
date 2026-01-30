use crate::agent::config::resolver::AgentConfigResolver;
use crate::api::repo::prompts::SurrealPromptRepo;

use super::repository::PromptRepository;

#[derive(Debug, Clone)]
pub struct ResolvedPrompt {
    pub template: String,
    pub model: Option<String>,
}

#[derive(Clone)]
pub struct PromptResolver {
    prompt_repo: SurrealPromptRepo,
    config_resolver: AgentConfigResolver,
}

impl PromptResolver {
    pub fn new(prompt_repo: SurrealPromptRepo, config_resolver: AgentConfigResolver) -> Self {
        Self {
            prompt_repo,
            config_resolver,
        }
    }

    pub async fn resolve(&self, agent_id: &str, name: &str) -> Option<ResolvedPrompt> {
        if let Ok(Some(p)) = self.prompt_repo.find_by_agent_and_name(agent_id, name).await {
            return Some(ResolvedPrompt {
                template: p.template,
                model: p.model,
            });
        }

        self.config_resolver
            .resolve_fs(agent_id, "prompts", name)
            .map(|entry| ResolvedPrompt {
                template: entry.template,
                model: entry.metadata.get("model").cloned(),
            })
    }
}
