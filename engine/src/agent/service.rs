use crate::api::repo::agents::SurrealAgentRepo;
use crate::error::AppError;
use crate::repository::Repository;
use crate::tool::configurable_tools;

use super::dto::{AgentResponse, CreateAgentRequest, UpdateAgentRequest};
use super::models::Agent;
use super::repository::AgentRepository;

#[derive(Clone)]
pub struct AgentService {
    repo: SurrealAgentRepo,
}

impl AgentService {
    pub fn new(repo: SurrealAgentRepo) -> Self {
        Self { repo }
    }

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateAgentRequest,
    ) -> Result<AgentResponse, AppError> {
        let now = chrono::Utc::now();
        let tools = req.tools.unwrap_or_else(|| configurable_tools().to_vec());

        let agent = Agent {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: Some(user_id.to_string()),
            name: req.name,
            description: req.description,
            system_prompt: req.system_prompt,
            model_group: req.model_group.unwrap_or_else(|| "primary".to_string()),
            enabled: true,
            tools,
            sandbox_config: req.sandbox_config,
            created_at: now,
            updated_at: now,
        };

        let agent = self.repo.create(&agent).await?;
        Ok(agent.into())
    }

    pub async fn get(
        &self,
        user_id: &str,
        agent_id: &str,
    ) -> Result<AgentResponse, AppError> {
        let agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        if agent.user_id.as_deref().is_some_and(|id| id != user_id) {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        Ok(agent.into())
    }

    pub async fn list(
        &self,
        user_id: &str,
    ) -> Result<Vec<AgentResponse>, AppError> {
        let agents = self.repo.find_by_user_id(user_id).await?;
        Ok(agents.into_iter().map(Into::into).collect())
    }

    pub async fn update(
        &self,
        user_id: &str,
        agent_id: &str,
        req: UpdateAgentRequest,
    ) -> Result<AgentResponse, AppError> {
        let mut agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        if agent.user_id.as_deref().is_some_and(|id| id != user_id) {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        if let Some(name) = req.name {
            agent.name = name;
        }
        if let Some(description) = req.description {
            agent.description = description;
        }
        if let Some(system_prompt) = req.system_prompt {
            agent.system_prompt = system_prompt;
        }
        if let Some(model_group) = req.model_group {
            agent.model_group = model_group;
        }
        if let Some(enabled) = req.enabled {
            agent.enabled = enabled;
        }
        if let Some(tools) = req.tools {
            agent.tools = tools;
        }
        if let Some(sandbox_config) = req.sandbox_config {
            agent.sandbox_config = Some(sandbox_config);
        }
        agent.updated_at = chrono::Utc::now();

        let agent = self.repo.update(&agent).await?;
        Ok(agent.into())
    }

    pub async fn delete(
        &self,
        user_id: &str,
        agent_id: &str,
    ) -> Result<(), AppError> {
        let agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        if agent.user_id.as_deref().is_some_and(|id| id != user_id) {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        self.repo.delete(agent_id).await
    }
}
