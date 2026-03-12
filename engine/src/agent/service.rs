use chrono::{DateTime, Utc};

use crate::core::config::CacheConfig;
use crate::db::repo::agents::SurrealAgentRepo;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::tool::configurable_tools;

use super::models::{AgentResponse, CreateAgentRequest, UpdateAgentRequest};
use super::models::Agent;
use super::repository::AgentRepository;

#[derive(Clone)]
pub struct AgentService {
    repo: SurrealAgentRepo,
    cache: moka::future::Cache<String, Agent>,
}

impl AgentService {
    pub fn new(repo: SurrealAgentRepo, cache_config: &CacheConfig) -> Self {
        let cache = moka::future::Cache::builder()
            .max_capacity(cache_config.entity_max_capacity)
            .time_to_live(std::time::Duration::from_secs(cache_config.entity_ttl_secs))
            .build();
        Self { repo, cache }
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
            model_group: req.model_group.unwrap_or_else(|| "primary".to_string()),
            enabled: true,
            tools,
            sandbox_config: req.sandbox_config,
            max_concurrent_tasks: None,
            avatar: None,
            identity: std::collections::BTreeMap::new(),
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        };

        let agent = self.repo.create(&agent).await?;
        Ok(agent.into())
    }

    pub async fn find_by_id(&self, agent_id: &str) -> Result<Option<Agent>, AppError> {
        if let Some(agent) = self.cache.get(agent_id).await {
            return Ok(Some(agent));
        }
        let result = self.repo.find_by_id(agent_id).await?;
        if let Some(ref agent) = result {
            self.cache.insert(agent_id.to_string(), agent.clone()).await;
        }
        Ok(result)
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
        self.cache.invalidate(agent_id).await;
        Ok(agent.into())
    }

    pub async fn find_by_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<super::models::Agent>, AppError> {
        self.repo.find_by_name(user_id, name).await
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

        self.cache.invalidate(agent_id).await;
        self.repo.delete(agent_id).await
    }

    pub async fn find_due_heartbeats(&self, now: DateTime<Utc>) -> Result<Vec<Agent>, AppError> {
        self.repo.find_due_heartbeats(now).await
    }

    pub async fn update_next_heartbeat(
        &self,
        agent_id: &str,
        next: Option<DateTime<Utc>>,
    ) -> Result<Agent, AppError> {
        let mut agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        agent.next_heartbeat_at = next;
        agent.updated_at = chrono::Utc::now();
        let agent = self.repo.update(&agent).await?;
        self.cache.invalidate(agent_id).await;
        Ok(agent)
    }

    pub async fn update_heartbeat_chat(
        &self,
        agent_id: &str,
        chat_id: &str,
    ) -> Result<Agent, AppError> {
        let mut agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        agent.heartbeat_chat_id = Some(chat_id.to_string());
        agent.updated_at = chrono::Utc::now();
        let agent = self.repo.update(&agent).await?;
        self.cache.invalidate(agent_id).await;
        Ok(agent)
    }

    pub async fn set_heartbeat(
        &self,
        agent_id: &str,
        interval_minutes: Option<u64>,
    ) -> Result<Agent, AppError> {
        let mut agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        agent.heartbeat_interval = interval_minutes;
        match interval_minutes {
            Some(mins) if mins > 0 => {
                agent.next_heartbeat_at = Some(Utc::now() + chrono::Duration::minutes(mins as i64));
            }
            _ => {
                agent.next_heartbeat_at = None;
            }
        }
        agent.updated_at = Utc::now();
        let agent = self.repo.update(&agent).await?;
        self.cache.invalidate(agent_id).await;
        Ok(agent)
    }
}
