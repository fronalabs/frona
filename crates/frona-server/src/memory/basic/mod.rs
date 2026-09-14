//! The basic memory service: user/agent/space "blob" memory. Self-contained
//! module - its models, repository traits, and store-memory tools live here.

pub mod models;
pub mod repository;
pub mod tools;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rig_core::completion::Message as RigMessage;

use crate::agent::prompt::PromptLoader;
use crate::chat::repository::ChatRepository;
use crate::core::config::MemoryConfig;
use crate::core::error::AppError;
use crate::core::repository::Repository;
use crate::db::repo::basic_memory::SurrealMemoryEntryRepo;
use crate::db::repo::basic_memory::SurrealMemoryRepo;
use crate::db::repo::chats::SurrealChatRepo;
use crate::db::repo::spaces::SurrealSpaceRepo;
use crate::inference::ModelGroup;
use crate::inference::context::estimate_tokens;
use crate::inference::provider::service::ModelProviderService;
use crate::inference::text_inference;
use crate::memory::basic::models::{Memory, MemoryEntry, MemorySourceType};
use crate::memory::basic::repository::{MemoryEntryRepository, MemoryRepository};
use crate::memory::service::{MemoryContext, MemoryService};
use crate::scheduler::Scheduler;
use crate::space::repository::SpaceRepository;
use crate::tool::AgentTool;

/// The basic memory service: user/agent/space "blob" memory - stored entries
/// rolled into a compacted summary, injected via `retrieve`. Implements the
/// `MemoryService` trait; compaction is inherent (registered with the scheduler
/// via `register_maintenance`).
#[derive(Clone)]
pub struct BasicMemoryService {
    memory_repo: SurrealMemoryRepo,
    memory_entry_repo: SurrealMemoryEntryRepo,
    /// Space + chat access for the space-summarization sweep - held explicitly so
    /// this module's data surface is visible, not reached through a raw db handle.
    space_repo: SurrealSpaceRepo,
    chat_repo: SurrealChatRepo,
    model_providers: Arc<ModelProviderService>,
    prompts: PromptLoader,
    usage_service: crate::inference::usage::UsageService,
    memory_config: MemoryConfig,
}

impl BasicMemoryService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory_repo: SurrealMemoryRepo,
        memory_entry_repo: SurrealMemoryEntryRepo,
        space_repo: SurrealSpaceRepo,
        chat_repo: SurrealChatRepo,
        model_providers: Arc<ModelProviderService>,
        prompts: PromptLoader,
        usage_service: crate::inference::usage::UsageService,
        memory_config: MemoryConfig,
    ) -> Self {
        Self {
            memory_repo,
            memory_entry_repo,
            space_repo,
            chat_repo,
            model_providers,
            prompts,
            usage_service,
            memory_config,
        }
    }

    /// Prefer the memory group; otherwise use the model selected for the chat.
    fn compaction_model_group(&self, chat_model_group: &str) -> Result<ModelGroup, AppError> {
        match self.model_providers.resolve(&crate::inference::ModelRef(
            self.memory_config.model_group.clone().into(),
        )) {
            Ok(group) => Ok(group),
            Err(crate::inference::InferenceError::ModelGroupNotFound(_)) => {
                Ok(self.model_providers.resolve(&crate::inference::ModelRef(
                    chat_model_group.to_owned().into(),
                ))?)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// User and space memories can span chats. Use the most recently active
    /// chat whose agent still exists, unless a memory group is configured.
    async fn compaction_model_group_for_chats(
        &self,
        agent_service: &crate::agent::service::AgentService,
        chats: &[crate::chat::models::Chat],
    ) -> Result<ModelGroup, AppError> {
        match self.model_providers.resolve(&crate::inference::ModelRef(
            self.memory_config.model_group.clone().into(),
        )) {
            Ok(group) => return Ok(group),
            Err(crate::inference::InferenceError::ModelGroupNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        for chat in chats {
            if let Some(agent) = agent_service.find_by_id(&chat.agent_id).await? {
                return self.compaction_model_group(&agent.model_group);
            }
        }
        Err(AppError::Validation(
            "No memory model group or chat model available for compaction".into(),
        ))
    }

    /// Load a compaction prompt from the resources dir. Missing → `AppError`
    /// (a packaging/deploy problem) rather than a panic on the background task.
    fn load_prompt(&self, name: &str) -> Result<String, AppError> {
        self.prompts.read(name).ok_or_else(|| {
            AppError::Internal(format!("compaction prompt {name} missing from resources"))
        })
    }

    pub async fn store_memory_entry(
        &self,
        agent_id: &str,
        content: &str,
        source_chat_id: Option<&str>,
    ) -> Result<MemoryEntry, AppError> {
        tracing::debug!(agent_id = %agent_id, content = %content, "Storing agent memory entry");

        let entry = MemoryEntry {
            id: crate::core::repository::new_id(),
            agent_id: agent_id.to_string(),
            user_id: None,
            content: content.to_string(),
            source_chat_id: source_chat_id.map(|s| s.to_string()),
            created_at: Utc::now(),
        };

        self.memory_entry_repo.create(&entry).await
    }

    pub async fn store_user_memory_entry(
        &self,
        user_id: &str,
        content: &str,
        source_chat_id: Option<&str>,
    ) -> Result<MemoryEntry, AppError> {
        tracing::debug!(user_id = %user_id, content = %content, "Storing user memory entry");

        let entry = MemoryEntry {
            id: crate::core::repository::new_id(),
            agent_id: String::new(),
            user_id: Some(user_id.to_string()),
            content: content.to_string(),
            source_chat_id: source_chat_id.map(|s| s.to_string()),
            created_at: Utc::now(),
        };

        self.memory_entry_repo.create(&entry).await
    }

    pub async fn compact_entries_if_needed(
        &self,
        user_id: &str,
        agent_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let entries = self.memory_entry_repo.find_by_agent_id(agent_id).await?;
        let total_tokens: usize = entries.iter().map(|e| estimate_tokens(&e.content)).sum();

        if total_tokens <= self.memory_config.basic_compaction_token_threshold {
            tracing::debug!(
                agent_id = %agent_id,
                token_count = total_tokens,
                threshold = self.memory_config.basic_compaction_token_threshold,
                "Skipping memory compaction (below threshold)"
            );
            return Ok(());
        }

        self.compact_entries(
            user_id,
            agent_id,
            MemorySourceType::Agent,
            entries,
            compaction_model_group,
        )
        .await
    }

    pub async fn compact_entries_forced(
        &self,
        user_id: &str,
        agent_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let entries = self.memory_entry_repo.find_by_agent_id(agent_id).await?;
        if entries.is_empty() {
            return Ok(());
        }
        self.compact_entries(
            user_id,
            agent_id,
            MemorySourceType::Agent,
            entries,
            compaction_model_group,
        )
        .await
    }

    pub async fn compact_user_entries_if_needed(
        &self,
        user_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let entries = self.memory_entry_repo.find_by_user_id(user_id).await?;
        let total_tokens: usize = entries.iter().map(|e| estimate_tokens(&e.content)).sum();

        if total_tokens <= self.memory_config.basic_compaction_token_threshold {
            tracing::debug!(
                user_id = %user_id,
                token_count = total_tokens,
                threshold = self.memory_config.basic_compaction_token_threshold,
                "Skipping user memory compaction (below threshold)"
            );
            return Ok(());
        }

        self.compact_entries(
            user_id,
            user_id,
            MemorySourceType::User,
            entries,
            compaction_model_group,
        )
        .await
    }

    pub async fn compact_user_entries_forced(
        &self,
        user_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let entries = self.memory_entry_repo.find_by_user_id(user_id).await?;
        if entries.is_empty() {
            return Ok(());
        }
        self.compact_entries(
            user_id,
            user_id,
            MemorySourceType::User,
            entries,
            compaction_model_group,
        )
        .await
    }

    async fn compact_entries(
        &self,
        user_id: &str,
        source_id: &str,
        source_type: MemorySourceType,
        entries: Vec<MemoryEntry>,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        // User entries are user-scoped; agent/space/chat entries are source-scoped -
        // this drives the input label and which delete-before the sweep runs.
        let is_user = matches!(source_type, MemorySourceType::User);
        let scope = if is_user { "user" } else { "agent" };
        let token_count_before: usize = entries.iter().map(|e| estimate_tokens(&e.content)).sum();
        tracing::info!(
            source_id = %source_id,
            entry_count = entries.len(),
            token_count = token_count_before,
            "Running memory compaction"
        );

        let existing_memory = self
            .memory_repo
            .find_latest(source_type.clone(), source_id)
            .await?;

        let mut compaction_input = String::new();
        if let Some(ref mem) = existing_memory {
            compaction_input.push_str(&format!("Previous {scope} memory:\n"));
            compaction_input.push_str(&mem.content);
            compaction_input.push_str("\n\nNew memories to incorporate:\n");
        }
        for entry in &entries {
            compaction_input.push_str(&format!("- {}\n", entry.content));
        }

        let prompt = self.load_prompt("MEMORY_COMPACTION.md")?;
        let target = match source_type {
            MemorySourceType::Agent => crate::inference::usage::CompactionTarget::Agent {
                agent_id: source_id.to_string(),
            },
            MemorySourceType::Space => crate::inference::usage::CompactionTarget::Space {
                space_id: source_id.to_string(),
            },
            MemorySourceType::Chat | MemorySourceType::User => {
                crate::inference::usage::CompactionTarget::User
            }
        };
        let usage_ctx = crate::inference::usage::UsageContext::new(
            crate::inference::usage::InferenceKind::Compaction { target },
            user_id,
            compaction_model_group.name.clone(),
        );
        let summary = text_inference(
            compaction_model_group,
            &prompt,
            vec![RigMessage::user(&compaction_input)],
            &self.usage_service,
            &usage_ctx,
        )
        .await
        .map_err(|e| AppError::Internal(format!("Memory compaction failed: {e}")))?;

        let token_count_after = estimate_tokens(&summary);
        tracing::info!(
            source_id = %source_id,
            token_count_before,
            token_count_after,
            "Memory compaction complete"
        );

        let now = Utc::now();
        let last_entry_time = entries.last().map(|e| e.created_at).unwrap_or(now);

        let memory = Memory {
            id: existing_memory
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(crate::core::repository::new_id),
            source_type,
            source_id: source_id.to_string(),
            content: summary,
            metadata: serde_json::json!({
                "compacted_until": last_entry_time,
                "item_count": entries.len(),
            }),
            created_at: existing_memory
                .as_ref()
                .map(|m| m.created_at)
                .unwrap_or(now),
            updated_at: now,
        };

        if existing_memory.is_some() {
            self.memory_repo.update(&memory).await?;
        } else {
            self.memory_repo.create(&memory).await?;
        }

        if is_user {
            self.memory_entry_repo
                .delete_by_user_id_before(source_id, last_entry_time)
                .await?;
        } else {
            self.memory_entry_repo
                .delete_by_agent_id_before(source_id, last_entry_time)
                .await?;
        }

        Ok(())
    }

    pub async fn compact_space(
        &self,
        user_id: &str,
        space_id: &str,
        chat_summaries: Vec<(String, String)>,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        if chat_summaries.is_empty() {
            return Ok(());
        }

        let mut input = String::new();
        for (title, summary) in &chat_summaries {
            input.push_str(&format!("## {title}\n{summary}\n\n"));
        }

        let prompt = self.load_prompt("SPACE_COMPACTION.md")?;
        let usage_ctx = crate::inference::usage::UsageContext::new(
            crate::inference::usage::InferenceKind::Compaction {
                target: crate::inference::usage::CompactionTarget::Space {
                    space_id: space_id.to_string(),
                },
            },
            user_id,
            compaction_model_group.name.clone(),
        );
        let summary = text_inference(
            compaction_model_group,
            &prompt,
            vec![RigMessage::user(&input)],
            &self.usage_service,
            &usage_ctx,
        )
        .await
        .map_err(|e| AppError::Internal(format!("Space compaction failed: {e}")))?;

        let now = Utc::now();
        let existing_memory = self
            .memory_repo
            .find_latest(MemorySourceType::Space, space_id)
            .await?;

        let memory = Memory {
            id: existing_memory
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(crate::core::repository::new_id),
            source_type: MemorySourceType::Space,
            source_id: space_id.to_string(),
            content: summary,
            metadata: serde_json::json!({
                "chat_count": chat_summaries.len(),
            }),
            created_at: existing_memory
                .as_ref()
                .map(|m| m.created_at)
                .unwrap_or(now),
            updated_at: now,
        };

        if existing_memory.is_some() {
            self.memory_repo.update(&memory).await?;
        } else {
            self.memory_repo.create(&memory).await?;
        }

        Ok(())
    }

    pub async fn get_memory(
        &self,
        source_type: MemorySourceType,
        source_id: &str,
    ) -> Result<Option<Memory>, AppError> {
        self.memory_repo.find_latest(source_type, source_id).await
    }
}

impl BasicMemoryService {
    async fn run_agent_sweep(
        &self,
        agent_service: &crate::agent::service::AgentService,
    ) -> Result<(), AppError> {
        let ids = self.memory_entry_repo.find_distinct_agent_ids().await?;
        for id in &ids {
            // Resolve the agent's owning user for the usage row; skip if gone.
            match agent_service.find_by_id(id).await {
                Ok(Some(agent)) => {
                    let result = async {
                        let group = self.compaction_model_group(&agent.model_group)?;
                        self.compact_entries_if_needed(&agent.user_id, id, &group)
                            .await
                    }
                    .await;
                    if let Err(e) = result {
                        tracing::warn!(agent_id = %id, error = %e, "agent memory compaction failed");
                    }
                }
                Ok(None) => tracing::debug!(agent_id = %id, "agent gone; skipping compaction"),
                Err(e) => tracing::warn!(agent_id = %id, error = %e, "failed to load agent"),
            }
        }
        Ok(())
    }

    async fn run_user_sweep(
        &self,
        agent_service: &crate::agent::service::AgentService,
    ) -> Result<(), AppError> {
        let ids = self.memory_entry_repo.find_distinct_user_ids().await?;
        for id in &ids {
            let result = async {
                let chats = self.chat_repo.find_by_user_id(id).await?;
                let group = self
                    .compaction_model_group_for_chats(agent_service, &chats)
                    .await?;
                self.compact_user_entries_if_needed(id, &group).await
            }
            .await;
            if let Err(e) = result {
                tracing::warn!(user_id = %id, error = %e, "user memory compaction failed");
            }
        }
        Ok(())
    }

    async fn run_space_sweep(
        &self,
        chat_service: &crate::chat::service::ChatService,
        agent_service: &crate::agent::service::AgentService,
    ) -> Result<(), AppError> {
        for space in self.space_repo.find_all().await? {
            let chats = self.chat_repo.find_by_space_id(&space.id).await?;
            if chats.is_empty() {
                continue;
            }
            let mut summaries = Vec::new();
            for chat in &chats {
                let title = chat.title.clone().unwrap_or_else(|| "Untitled".to_string());
                let summary = chat_service
                    .compactor()
                    .load_conversation(&chat.id)
                    .await?
                    .summary
                    .unwrap_or_else(|| format!("(No summary available for chat: {title})"));
                summaries.push((title, summary));
            }
            let result = async {
                let group = self
                    .compaction_model_group_for_chats(agent_service, &chats)
                    .await?;
                self.compact_space(&space.user_id, &space.id, summaries, &group)
                    .await
            }
            .await;
            if let Err(e) = result {
                tracing::warn!(space_id = %space.id, error = %e, "failed to compact space");
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MemoryService for BasicMemoryService {
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![
            Arc::new(crate::memory::basic::tools::StoreAgentMemoryTool::new(
                self.clone(),
                self.prompts.clone(),
            )),
            Arc::new(crate::memory::basic::tools::StoreUserMemoryTool::new(
                self.clone(),
                self.prompts.clone(),
            )),
        ]
    }

    async fn retrieve(&self, mcx: &mut MemoryContext<'_>) -> Result<(), AppError> {
        // Static usage section first - read fresh so prompt edits apply live;
        // constant across turns, so `[head][section]` stays a cacheable prefix
        // and the dynamic memory tags follow.
        let section = self.prompts.read("MEMORY.md").unwrap_or_default();
        if !section.is_empty() {
            mcx.system_prompt.push_str("\n\n");
            mcx.system_prompt.push_str(&section);
        }

        let user_id = mcx.ctx.user.id.clone();
        let agent_id = mcx.ctx.agent.id.clone();
        let space_id = mcx.ctx.chat.as_ref().and_then(|c| c.space_id.clone());

        // <space_context>
        if let Some(sid) = space_id.as_deref()
            && let Some(space_mem) = self.get_memory(MemorySourceType::Space, sid).await?
        {
            mcx.system_prompt.push_str("\n\n<space_context>\n");
            mcx.system_prompt.push_str(&space_mem.content);
            mcx.system_prompt.push_str("\n</space_context>");
        }

        // <user_memory>: compacted summary + entries after compacted_until, else raw.
        if let Some(mem) = self.get_memory(MemorySourceType::User, &user_id).await? {
            mcx.system_prompt.push_str("\n\n<user_memory>\n");
            mcx.system_prompt.push_str(&mem.content);
            let until = compacted_until(&mem);
            let entries = match until {
                Some(t) => {
                    self.memory_entry_repo
                        .find_by_user_id_after(&user_id, t)
                        .await?
                }
                None => self.memory_entry_repo.find_by_user_id(&user_id).await?,
            };
            if !entries.is_empty() {
                mcx.system_prompt.push('\n');
                for e in &entries {
                    mcx.system_prompt.push_str(&format!("- {}\n", e.content));
                }
            }
            mcx.system_prompt.push_str("</user_memory>");
        } else {
            let entries = self.memory_entry_repo.find_by_user_id(&user_id).await?;
            if !entries.is_empty() {
                mcx.system_prompt.push_str("\n\n<user_memory>\n");
                for e in &entries {
                    mcx.system_prompt.push_str(&format!("- {}\n", e.content));
                }
                mcx.system_prompt.push_str("</user_memory>");
            }
        }

        // <agent_memory>
        if let Some(mem) = self.get_memory(MemorySourceType::Agent, &agent_id).await? {
            mcx.system_prompt.push_str("\n\n<agent_memory>\n");
            mcx.system_prompt.push_str(&mem.content);
            let until = compacted_until(&mem);
            let entries = match until {
                Some(t) => {
                    self.memory_entry_repo
                        .find_by_agent_id_after(&agent_id, t)
                        .await?
                }
                None => self.memory_entry_repo.find_by_agent_id(&agent_id).await?,
            };
            if !entries.is_empty() {
                mcx.system_prompt.push('\n');
                for e in &entries {
                    mcx.system_prompt.push_str(&format!("- {}\n", e.content));
                }
            }
            mcx.system_prompt.push_str("</agent_memory>");
        } else {
            let entries = self.memory_entry_repo.find_by_agent_id(&agent_id).await?;
            if !entries.is_empty() {
                mcx.system_prompt.push_str("\n\n<agent_memory>\n");
                for e in &entries {
                    mcx.system_prompt.push_str(&format!("- {}\n", e.content));
                }
                mcx.system_prompt.push_str("</agent_memory>");
            }
        }
        Ok(())
    }

    fn register_maintenance(&self, scheduler: &Scheduler) {
        let cfg = &scheduler.app_state.config.memory;
        let memory_interval = Duration::from_secs(cfg.basic_compaction_secs);
        let space_interval = Duration::from_secs(cfg.basic_space_compaction_secs);
        tracing::info!(
            memory_secs = cfg.basic_compaction_secs,
            space_secs = cfg.basic_space_compaction_secs,
            "basic memory maintenance registered"
        );

        let me = self.clone();
        let agents = scheduler.app_state.agent_service.clone();
        scheduler.register_periodic(memory_interval, "memory_compaction", move || {
            let me = me.clone();
            let agents = agents.clone();
            async move { me.run_agent_sweep(&agents).await }
        });

        let me = self.clone();
        let agents = scheduler.app_state.agent_service.clone();
        scheduler.register_periodic(memory_interval, "user_memory_compaction", move || {
            let me = me.clone();
            let agents = agents.clone();
            async move { me.run_user_sweep(&agents).await }
        });

        let me = self.clone();
        let chats = scheduler.app_state.chat_service.clone();
        let agents = scheduler.app_state.agent_service.clone();
        scheduler.register_periodic(space_interval, "space_compaction", move || {
            let me = me.clone();
            let chats = chats.clone();
            let agents = agents.clone();
            async move { me.run_space_sweep(&chats, &agents).await }
        });
    }
}

/// Parse `metadata["compacted_until"]` from a compacted `Memory` row.
fn compacted_until(mem: &Memory) -> Option<DateTime<Utc>> {
    mem.metadata
        .get("compacted_until")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
}
