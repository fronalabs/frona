use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::core::config::CacheConfig;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::db::repo::agents::SurrealAgentRepo;
use crate::policy::sandbox::SandboxPolicy;
use crate::policy::service::PolicyService;
use crate::tool::sandbox::driver::resource_monitor::SystemResourceManager;

use super::config::parse_frontmatter;
use super::models::Agent;
use super::models::{CreateAgentRequest, UpdateAgentRequest};
use super::repository::AgentRepository;
use crate::auth::UserService;
use crate::core::Handle;
use crate::storage::StorageService;

#[derive(Clone)]
pub struct AgentService {
    repo: SurrealAgentRepo,
    cache: moka::future::Cache<String, Agent>,
    resource_manager: Arc<SystemResourceManager>,
    policy_service: PolicyService,
    user_service: UserService,
}

impl AgentService {
    pub fn new(
        repo: SurrealAgentRepo,
        cache_config: &CacheConfig,
        resource_manager: Arc<SystemResourceManager>,
        policy_service: PolicyService,
        user_service: UserService,
    ) -> Self {
        let cache = moka::future::Cache::builder()
            .max_capacity(cache_config.entity_max_capacity)
            .time_to_live(std::time::Duration::from_secs(cache_config.entity_ttl_secs))
            .build();
        Self {
            repo,
            cache,
            resource_manager,
            policy_service,
            user_service,
        }
    }

    pub async fn sync_agent_limits(&self) -> Result<(), AppError> {
        let agents = self.repo.find_all().await?;
        for agent in agents {
            if let Some(ref limits) = agent.sandbox_limits {
                self.resource_manager.set_agent_limits(
                    &agent.id,
                    Some(limits.max_cpu_pct),
                    Some(limits.max_memory_pct),
                );
            }
        }
        Ok(())
    }

    fn push_agent_limits(&self, agent_id: &str, agent: &Agent) {
        if let Some(ref limits) = agent.sandbox_limits {
            self.resource_manager.set_agent_limits(
                agent_id,
                Some(limits.max_cpu_pct),
                Some(limits.max_memory_pct),
            );
        }
    }

    pub async fn create(&self, user_id: &str, req: CreateAgentRequest) -> Result<Agent, AppError> {
        if req.model_group.is_empty() {
            return Err(AppError::Validation("model_group must not be empty".into()));
        }

        let raw_handle = req
            .handle
            .clone()
            .or_else(|| req.id.clone())
            .unwrap_or_else(|| slugify_handle(&req.name));
        let handle = Handle::try_new(raw_handle)
            .map_err(|e| AppError::Validation(format!("invalid agent handle: {e}")))?;
        if self.repo.find_by_handle(user_id, &handle).await?.is_some() {
            return Err(AppError::Validation(format!(
                "Agent with handle '{handle}' already exists"
            )));
        }

        let id = crate::core::repository::new_id();
        let now = chrono::Utc::now();

        let agent = Agent {
            id,
            user_id: user_id.to_string(),
            handle,
            name: req.name,
            description: req.description,
            model_group: req.model_group,
            enabled: true,
            skills: req.skills,
            sandbox_limits: req.sandbox_limits,
            max_concurrent_tasks: None,
            avatar: None,
            identity: std::collections::BTreeMap::new(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        };

        let agent = self.repo.create(&agent).await?;
        self.push_agent_limits(&agent.id, &agent);
        // Reconcile with default-empty so a recycled agent id clears stale rows.
        let policy = req.sandbox_policy.as_ref().cloned().unwrap_or_default();
        let user_handle = self.user_service.handle_of(user_id).await?;
        self.policy_service
            .reconcile_sandbox_policy(
                user_id,
                crate::policy::reconcile::EntityRef::agent(&user_handle, &agent.handle),
                &policy,
            )
            .await?;
        Ok(agent)
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

    pub async fn get(&self, user_id: &str, agent_id: &str) -> Result<Agent, AppError> {
        let agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        if agent.user_id != user_id {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        Ok(agent)
    }

    /// Tries handle lookup first, falls back to UUID for call-sites passing `agent.id`.
    pub async fn owned_by(&self, user_id: &str, handle_or_id: &str) -> Result<Agent, AppError> {
        if let Some(agent) = self.find_by_handle(user_id, handle_or_id).await? {
            return Ok(agent);
        }
        self.get(user_id, handle_or_id).await
    }

    /// Invalid handle returns `None` (not an error) so callers can chain a fallback.
    pub async fn find_by_handle(
        &self,
        user_id: &str,
        handle: &str,
    ) -> Result<Option<Agent>, AppError> {
        let Ok(handle) = Handle::try_new(handle) else {
            return Ok(None);
        };
        self.repo.find_by_handle(user_id, &handle).await
    }

    /// Resolve the user's `system` builtin agent - the identity for detached /
    /// background inference that has no originating chat (e.g. the PKM sync
    /// investigator). Cloned for every user at signup, so absence is an invariant
    /// violation, not a normal outcome → `NotFound`.
    pub async fn system_agent(&self, user_id: &str) -> Result<Agent, AppError> {
        self.find_by_handle(user_id, crate::agent::models::SYSTEM_AGENT_HANDLE)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("system agent not found for user {user_id}")))
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<Agent>, AppError> {
        self.repo.find_by_user_id(user_id).await
    }

    pub async fn heartbeat_chat_ids(&self, user_id: &str) -> Vec<String> {
        self.repo
            .find_by_user_id(user_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| a.heartbeat_chat_id)
            .collect()
    }

    pub async fn update(
        &self,
        user_id: &str,
        agent_id: &str,
        req: UpdateAgentRequest,
    ) -> Result<Agent, AppError> {
        let mut agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        if agent.user_id != user_id {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        let explicit_name = req.name.is_some();
        if let Some(ref name) = req.name {
            agent.name = name.clone();
            agent.identity.insert("name".to_string(), name.clone());
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
        if let Some(skills) = req.skills {
            if skills.len() == 1 && skills[0] == "*" {
                agent.skills = None;
            } else {
                agent.skills = Some(skills);
            }
        }
        if let Some(sandbox_limits) = req.sandbox_limits {
            agent.sandbox_limits = Some(sandbox_limits);
        }
        if let Some(prompt) = req.prompt {
            agent.prompt = if prompt.is_empty() {
                None
            } else {
                Some(prompt)
            };
        }
        if let Some(ref identity) = req.identity {
            if let Some(avatar) = identity.get("avatar")
                && avatar.starts_with("data:")
            {
                return Err(AppError::Validation(
                    "Inline data: URLs are not allowed for avatars".into(),
                ));
            }
            if !explicit_name
                && let Some(new_name) = identity.get("name").filter(|n| !n.is_empty())
                && agent.identity.get("name") != Some(new_name)
            {
                agent.name = new_name.clone();
            }
            agent.identity = req.identity.unwrap();
        }
        agent.updated_at = chrono::Utc::now();

        let agent = self.repo.update(&agent).await?;
        self.push_agent_limits(agent_id, &agent);
        self.cache.invalidate(agent_id).await;
        if let Some(policy) = req.sandbox_policy.as_ref() {
            let user_handle = self.user_service.handle_of(user_id).await?;
            self.policy_service
                .reconcile_sandbox_policy(
                    user_id,
                    crate::policy::reconcile::EntityRef::agent(&user_handle, &agent.handle),
                    policy,
                )
                .await?;
        }
        Ok(agent)
    }

    pub async fn find_by_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<super::models::Agent>, AppError> {
        self.repo.find_by_name(user_id, name).await
    }

    pub async fn delete(&self, user_id: &str, agent_id: &str) -> Result<(), AppError> {
        let agent = self
            .repo
            .find_by_id(agent_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Agent not found".into()))?;

        if agent.user_id != user_id {
            return Err(AppError::Forbidden("Not your agent".into()));
        }

        self.cache.invalidate(agent_id).await;
        let user_handle = self.user_service.handle_of(user_id).await?;
        self.policy_service
            .reconcile_sandbox_policy(
                user_id,
                crate::policy::reconcile::EntityRef::agent(&user_handle, &agent.handle),
                &SandboxPolicy::permissive(),
            )
            .await?;
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

    /// Idempotent: returns the existing row if the user already has this handle.
    pub async fn clone_builtin_for_user(
        &self,
        user_id: &str,
        handle: &Handle,
        storage: &StorageService,
    ) -> Result<Agent, AppError> {
        if let Some(existing) = self.repo.find_by_handle(user_id, handle).await? {
            return Ok(existing);
        }

        let ws = storage.builtin_template_workspace(handle);
        let (name, description, model_group) = ws
            .read("AGENT.md")
            .map(|content| {
                let entry = parse_frontmatter(&content);
                let nm = entry
                    .metadata
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| title_case(handle.as_str()));
                let desc = entry
                    .metadata
                    .get("description")
                    .cloned()
                    .unwrap_or_default();
                let mg = entry
                    .metadata
                    .get("model_group")
                    .cloned()
                    .unwrap_or_else(|| "primary".to_string());
                (nm, desc, mg)
            })
            .unwrap_or_else(|| {
                (
                    title_case(handle.as_str()),
                    String::new(),
                    "primary".to_string(),
                )
            });

        let now = chrono::Utc::now();
        let agent = Agent {
            id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            handle: handle.clone(),
            name,
            description,
            model_group,
            enabled: true,
            skills: None,
            sandbox_limits: None,
            max_concurrent_tasks: None,
            avatar: None,
            identity: std::collections::BTreeMap::new(),
            prompt: None,
            heartbeat_interval: None,
            next_heartbeat_at: None,
            heartbeat_chat_id: None,
            created_at: now,
            updated_at: now,
        };
        let agent = self.repo.create(&agent).await?;
        self.push_agent_limits(&agent.id, &agent);
        let user_handle = self.user_service.handle_of(user_id).await?;
        self.policy_service
            .reconcile_sandbox_policy(
                user_id,
                crate::policy::reconcile::EntityRef::agent(&user_handle, &agent.handle),
                &SandboxPolicy::default(),
            )
            .await?;
        Ok(agent)
    }

    pub async fn clone_all_builtins_for_user(
        &self,
        user_id: &str,
        storage: &StorageService,
    ) -> Result<(), AppError> {
        for handle in crate::agent::models::BUILTIN_HANDLES {
            if let Err(e) = self.clone_builtin_for_user(user_id, handle, storage).await {
                tracing::warn!(
                    user_id,
                    handle = %handle,
                    error = %e,
                    "Failed to clone builtin agent for user"
                );
            }
        }
        Ok(())
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

fn title_case(handle: &str) -> String {
    handle
        .split(['-', '_'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn slugify_handle(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = true;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        crate::core::repository::new_id()
    } else {
        trimmed
    }
}
