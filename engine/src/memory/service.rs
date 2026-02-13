use std::sync::Arc;

use chrono::{DateTime, Utc};
use rig::completion::Message as RigMessage;

use crate::agent::workspace::{AgentPromptLoader, AgentWorkspaceManager};
use crate::api::repo::insights::SurrealInsightRepo;
use crate::api::repo::memories::SurrealMemoryRepo;
use crate::api::repo::messages::SurrealMessageRepo;
use crate::chat::message::models::Message;
use crate::chat::message::repository::MessageRepository;
use crate::core::error::AppError;
use crate::inference::config::ModelGroup;
use crate::inference::context::{estimate_tokens, resolve_context_window};
use crate::inference::convert::to_rig_messages;
use crate::inference::fallback::inference_with_fallback;
use crate::inference::ModelProviderRegistry;
use crate::memory::insight::models::Insight;
use crate::memory::insight::repository::InsightRepository;
use crate::memory::models::{Memory, MemorySourceType};
use crate::memory::repository::MemoryRepository;
use crate::agent::prompt::PromptLoader;
use crate::core::repository::Repository;

const INSIGHT_COMPACTION_TOKEN_THRESHOLD: usize = 3_000;

#[derive(Clone)]
pub struct MemoryService {
    memory_repo: SurrealMemoryRepo,
    insight_repo: SurrealInsightRepo,
    message_repo: SurrealMessageRepo,
    provider_registry: Arc<ModelProviderRegistry>,
    prompts: PromptLoader,
    workspaces: AgentWorkspaceManager,
}

impl MemoryService {
    pub fn new(
        memory_repo: SurrealMemoryRepo,
        insight_repo: SurrealInsightRepo,
        message_repo: SurrealMessageRepo,
        provider_registry: Arc<ModelProviderRegistry>,
        prompts: PromptLoader,
        workspaces: AgentWorkspaceManager,
    ) -> Self {
        Self {
            memory_repo,
            insight_repo,
            message_repo,
            provider_registry,
            prompts,
            workspaces,
        }
    }

    fn load_prompt(&self, name: &str, agent_id: Option<&str>) -> Option<String> {
        if let Some(aid) = agent_id {
            let ws = self.workspaces.get(aid);
            let loader = AgentPromptLoader::new(&ws, &self.prompts);
            return loader.read(name);
        }
        self.prompts.read(name)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn compact_chat_if_needed(
        &self,
        chat_id: &str,
        chat_agent_id: &str,
        system_prompt: &str,
        model_id: &str,
        context_window: Option<usize>,
        max_output_tokens: usize,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let messages = self.message_repo.find_by_chat_id(chat_id).await?;
        if messages.is_empty() {
            return Ok(());
        }

        let rig_messages = to_rig_messages(&messages, chat_agent_id);
        let window = resolve_context_window(model_id, context_window);
        let available = window.saturating_sub(max_output_tokens);

        let mut total_tokens = estimate_tokens(system_prompt);
        for msg in &rig_messages {
            total_tokens += crate::inference::context::estimate_message_tokens(msg);
        }

        if total_tokens <= available * 80 / 100 {
            return Ok(());
        }

        let existing_memory = self
            .memory_repo
            .find_latest(MemorySourceType::Chat, chat_id)
            .await?;

        let target = available * 70 / 100;
        let mut summary_budget = estimate_tokens(system_prompt);
        if let Some(ref mem) = existing_memory {
            summary_budget += estimate_tokens(&mem.content);
        }

        let mut keep_from_idx = messages.len();
        let mut running = 0usize;
        for (i, msg) in rig_messages.iter().enumerate().rev() {
            let cost = crate::inference::context::estimate_message_tokens(msg);
            if running + cost + summary_budget > target {
                break;
            }
            running += cost;
            keep_from_idx = i;
        }

        if keep_from_idx == 0 {
            return Ok(());
        }

        let messages_to_compact = &messages[..keep_from_idx];

        let mut compaction_input = String::new();
        if let Some(ref mem) = existing_memory {
            compaction_input.push_str("Previous summary:\n");
            compaction_input.push_str(&mem.content);
            compaction_input.push_str("\n\nNew messages to incorporate:\n");
        }
        for msg in messages_to_compact {
            let role_str = match msg.role {
                crate::chat::message::models::MessageRole::User => "User",
                crate::chat::message::models::MessageRole::Agent => "Agent",
                crate::chat::message::models::MessageRole::ToolResult => "Tool",
                crate::chat::message::models::MessageRole::TaskCompletion => "System",
            };
            compaction_input.push_str(&format!("{role_str}: {}\n", msg.content));
        }

        let user_msg = RigMessage::user(&compaction_input);
        let prompt = self.load_prompt("CHAT_COMPACTION.md", None)
            .expect("built-in CHAT_COMPACTION.md missing");
        let summary = inference_with_fallback(
            &self.provider_registry,
            compaction_model_group,
            &prompt,
            vec![],
            user_msg,
        )
        .await
        .map_err(|e| AppError::Internal(format!("Chat compaction failed: {e}")))?;

        let now = Utc::now();
        let compacted_until = messages_to_compact
            .last()
            .map(|m| m.created_at)
            .unwrap_or(now);

        let memory = Memory {
            id: existing_memory
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            source_type: MemorySourceType::Chat,
            source_id: chat_id.to_string(),
            content: summary,
            metadata: serde_json::json!({
                "compacted_until": compacted_until,
                "item_count": messages_to_compact.len(),
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

        for msg in messages_to_compact {
            self.message_repo.delete(&msg.id).await?;
        }

        Ok(())
    }

    pub async fn store_insight(
        &self,
        agent_id: &str,
        content: &str,
        source_chat_id: Option<&str>,
    ) -> Result<Insight, AppError> {
        tracing::debug!(agent_id = %agent_id, insight = %content, "Storing agent insight");

        let insight = Insight {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            user_id: None,
            content: content.to_string(),
            source_chat_id: source_chat_id.map(|s| s.to_string()),
            created_at: Utc::now(),
        };

        self.insight_repo.create(&insight).await
    }

    pub async fn store_user_insight(
        &self,
        user_id: &str,
        content: &str,
        source_chat_id: Option<&str>,
    ) -> Result<Insight, AppError> {
        tracing::debug!(user_id = %user_id, insight = %content, "Storing user insight");

        let insight = Insight {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: String::new(),
            user_id: Some(user_id.to_string()),
            content: content.to_string(),
            source_chat_id: source_chat_id.map(|s| s.to_string()),
            created_at: Utc::now(),
        };

        self.insight_repo.create(&insight).await
    }

    pub async fn compact_insights_if_needed(
        &self,
        agent_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let insights = self.insight_repo.find_by_agent_id(agent_id).await?;
        let total_tokens: usize = insights.iter().map(|i| estimate_tokens(&i.content)).sum();

        if total_tokens <= INSIGHT_COMPACTION_TOKEN_THRESHOLD {
            tracing::debug!(
                agent_id = %agent_id,
                token_count = total_tokens,
                threshold = INSIGHT_COMPACTION_TOKEN_THRESHOLD,
                "Skipping insight compaction (below threshold)"
            );
            return Ok(());
        }

        self.compact_insights(agent_id, MemorySourceType::Agent, insights, compaction_model_group)
            .await
    }

    pub async fn compact_insights_forced(
        &self,
        agent_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let insights = self.insight_repo.find_by_agent_id(agent_id).await?;
        if insights.is_empty() {
            return Ok(());
        }
        self.compact_insights(agent_id, MemorySourceType::Agent, insights, compaction_model_group)
            .await
    }

    pub async fn compact_user_insights_if_needed(
        &self,
        user_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let insights = self.insight_repo.find_by_user_id(user_id).await?;
        let total_tokens: usize = insights.iter().map(|i| estimate_tokens(&i.content)).sum();

        if total_tokens <= INSIGHT_COMPACTION_TOKEN_THRESHOLD {
            tracing::debug!(
                user_id = %user_id,
                token_count = total_tokens,
                threshold = INSIGHT_COMPACTION_TOKEN_THRESHOLD,
                "Skipping user insight compaction (below threshold)"
            );
            return Ok(());
        }

        self.compact_user_insights(user_id, insights, compaction_model_group)
            .await
    }

    pub async fn compact_user_insights_forced(
        &self,
        user_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let insights = self.insight_repo.find_by_user_id(user_id).await?;
        if insights.is_empty() {
            return Ok(());
        }
        self.compact_user_insights(user_id, insights, compaction_model_group)
            .await
    }

    async fn compact_user_insights(
        &self,
        user_id: &str,
        insights: Vec<Insight>,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let token_count_before: usize = insights.iter().map(|i| estimate_tokens(&i.content)).sum();
        tracing::info!(
            user_id = %user_id,
            insight_count = insights.len(),
            token_count = token_count_before,
            "Running user insight compaction"
        );

        let existing_memory = self
            .memory_repo
            .find_latest(MemorySourceType::User, user_id)
            .await?;

        let mut compaction_input = String::new();
        if let Some(ref mem) = existing_memory {
            compaction_input.push_str("Previous user memory:\n");
            compaction_input.push_str(&mem.content);
            compaction_input.push_str("\n\nNew facts to incorporate:\n");
        }
        for insight in &insights {
            compaction_input.push_str(&format!("- {}\n", insight.content));
        }

        let user_msg = RigMessage::user(&compaction_input);
        let prompt = self.load_prompt("INSIGHT_COMPACTION.md", None)
            .expect("built-in INSIGHT_COMPACTION.md missing");
        let summary = inference_with_fallback(
            &self.provider_registry,
            compaction_model_group,
            &prompt,
            vec![],
            user_msg,
        )
        .await
        .map_err(|e| AppError::Internal(format!("User insight compaction failed: {e}")))?;

        let token_count_after = estimate_tokens(&summary);
        tracing::info!(
            user_id = %user_id,
            token_count_before,
            token_count_after,
            "User insight compaction complete"
        );

        let now = Utc::now();
        let last_insight_time = insights.last().map(|i| i.created_at).unwrap_or(now);

        let memory = Memory {
            id: existing_memory
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            source_type: MemorySourceType::User,
            source_id: user_id.to_string(),
            content: summary,
            metadata: serde_json::json!({
                "compacted_until": last_insight_time,
                "item_count": insights.len(),
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

        self.insight_repo
            .delete_by_user_id_before(user_id, last_insight_time)
            .await?;

        Ok(())
    }

    async fn compact_insights(
        &self,
        source_id: &str,
        source_type: MemorySourceType,
        insights: Vec<Insight>,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let token_count_before: usize = insights.iter().map(|i| estimate_tokens(&i.content)).sum();
        tracing::info!(
            source_id = %source_id,
            insight_count = insights.len(),
            token_count = token_count_before,
            "Running insight compaction"
        );

        let existing_memory = self
            .memory_repo
            .find_latest(source_type.clone(), source_id)
            .await?;

        let mut compaction_input = String::new();
        if let Some(ref mem) = existing_memory {
            compaction_input.push_str("Previous agent memory:\n");
            compaction_input.push_str(&mem.content);
            compaction_input.push_str("\n\nNew facts to incorporate:\n");
        }
        for insight in &insights {
            compaction_input.push_str(&format!("- {}\n", insight.content));
        }

        let user_msg = RigMessage::user(&compaction_input);
        let agent_id = if source_type == MemorySourceType::Agent { Some(source_id) } else { None };
        let prompt = self.load_prompt("INSIGHT_COMPACTION.md", agent_id)
            .expect("built-in INSIGHT_COMPACTION.md missing");
        let summary = inference_with_fallback(
            &self.provider_registry,
            compaction_model_group,
            &prompt,
            vec![],
            user_msg,
        )
        .await
        .map_err(|e| AppError::Internal(format!("Insight compaction failed: {e}")))?;

        let token_count_after = estimate_tokens(&summary);
        tracing::info!(
            source_id = %source_id,
            token_count_before,
            token_count_after,
            "Insight compaction complete"
        );

        let now = Utc::now();
        let last_insight_time = insights.last().map(|i| i.created_at).unwrap_or(now);

        let memory = Memory {
            id: existing_memory
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            source_type,
            source_id: source_id.to_string(),
            content: summary,
            metadata: serde_json::json!({
                "compacted_until": last_insight_time,
                "item_count": insights.len(),
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

        self.insight_repo
            .delete_by_agent_id_before(source_id, last_insight_time)
            .await?;

        Ok(())
    }

    pub async fn compact_space(
        &self,
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

        let user_msg = RigMessage::user(&input);
        let prompt = self.load_prompt("SPACE_COMPACTION.md", None)
            .expect("built-in SPACE_COMPACTION.md missing");
        let summary = inference_with_fallback(
            &self.provider_registry,
            compaction_model_group,
            &prompt,
            vec![],
            user_msg,
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
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
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

    pub async fn get_conversation_context(
        &self,
        chat_id: &str,
    ) -> Result<(Option<String>, Vec<Message>), AppError> {
        let memory = self
            .memory_repo
            .find_latest(MemorySourceType::Chat, chat_id)
            .await?;

        match memory {
            Some(mem) => {
                let compacted_until: Option<DateTime<Utc>> = mem
                    .metadata
                    .get("compacted_until")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok());

                let messages = match compacted_until {
                    Some(until) => {
                        self.message_repo
                            .find_by_chat_id(chat_id)
                            .await?
                            .into_iter()
                            .filter(|m| m.created_at > until)
                            .collect()
                    }
                    None => self.message_repo.find_by_chat_id(chat_id).await?,
                };

                Ok((Some(mem.content), messages))
            }
            None => {
                let messages = self.message_repo.find_by_chat_id(chat_id).await?;
                Ok((None, messages))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn build_augmented_system_prompt(
        &self,
        base_prompt: &str,
        agent_id: &str,
        user_id: &str,
        space_id: Option<&str>,
        skill_summaries: &[(String, String)],
        agent_summaries: &[(String, String)],
        identity: &std::collections::BTreeMap<String, String>,
    ) -> Result<String, AppError> {
        // Prompt is ordered static → almost-static → dynamic to maximise
        // the cacheable prefix for LLM prompt caching.

        // --- Static: base prompt + shared prompt files ---
        let mut result = base_prompt.to_string();

        const AGENT_PROMPTS: &[&str] = &["WORKSPACE.md", "TOOLS.md", "MEMORY.md", "SCHEDULING.md"];
        for name in AGENT_PROMPTS {
            if let Some(content) = self.prompts.read(name) {
                result.push_str("\n\n");
                result.push_str(&content);
            }
        }

        // --- Almost-static: identity, skills, agents ---
        if !identity.is_empty() {
            result.push_str("\n\n<agent_identity>\n");
            for (key, value) in identity {
                result.push_str(&format!("{key}: {value}\n"));
            }
            result.push_str("</agent_identity>");
        }

        const CORE_IDENTITY_KEYS: &[&str] = &["name", "creature", "vibe"];
        let has_core_identity = CORE_IDENTITY_KEYS
            .iter()
            .all(|core_key| identity.keys().any(|k| k.eq_ignore_ascii_case(core_key)));

        if !has_core_identity
            && let Some(identity_prompt) = self.load_prompt("IDENTITY.md", Some(agent_id))
        {
            result.push_str("\n\n");
            result.push_str(&identity_prompt);
        }

        if !skill_summaries.is_empty() {
            let header = self.prompts.read("AVAILABLE_SKILLS.md").unwrap_or_default();
            result.push_str("\n\n<available_skills>\n");
            result.push_str(header.trim());
            result.push('\n');
            for (name, description) in skill_summaries {
                result.push_str(&format!("- {name}: {description}\n"));
            }
            result.push_str("</available_skills>");
        }

        if !agent_summaries.is_empty() {
            let header = self.prompts.read("AVAILABLE_AGENTS.md").unwrap_or_default();
            result.push_str("\n\n<available_agents>\n");
            result.push_str(header.trim());
            result.push('\n');
            for (name, description) in agent_summaries {
                result.push_str(&format!("- {name}: {description}\n"));
            }
            result.push_str("</available_agents>");
        }

        // --- Dynamic: space context, user memory, agent memory ---
        if let Some(sid) = space_id
            && let Some(space_mem) = self
                .get_memory(MemorySourceType::Space, sid)
                .await?
        {
            result.push_str("\n\n<space_context>\n");
            result.push_str(&space_mem.content);
            result.push_str("\n</space_context>");
        }

        if let Some(user_mem) = self
            .get_memory(MemorySourceType::User, user_id)
            .await?
        {
            tracing::debug!(
                user_id = %user_id,
                memory_len = user_mem.content.len(),
                "Using compacted user memory"
            );
            result.push_str("\n\n<user_memory>\n");
            result.push_str(&user_mem.content);

            let compacted_until = user_mem
                .metadata
                .get("compacted_until")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<DateTime<Utc>>().ok());

            let new_insights = match compacted_until {
                Some(until) => {
                    self.insight_repo
                        .find_by_user_id_after(user_id, until)
                        .await?
                }
                None => self.insight_repo.find_by_user_id(user_id).await?,
            };
            if !new_insights.is_empty() {
                result.push('\n');
                for insight in &new_insights {
                    result.push_str(&format!("- {}\n", insight.content));
                }
            }

            result.push_str("</user_memory>");
        } else {
            let insights = self.insight_repo.find_by_user_id(user_id).await?;
            if !insights.is_empty() {
                tracing::debug!(
                    user_id = %user_id,
                    insight_count = insights.len(),
                    "No compacted user memory, using raw insights"
                );
                result.push_str("\n\n<user_memory>\n");
                for insight in &insights {
                    result.push_str(&format!("- {}\n", insight.content));
                }
                result.push_str("</user_memory>");
            }
        }

        if let Some(agent_mem) = self
            .get_memory(MemorySourceType::Agent, agent_id)
            .await?
        {
            tracing::debug!(
                agent_id = %agent_id,
                memory_len = agent_mem.content.len(),
                "Using compacted agent memory"
            );
            result.push_str("\n\n<agent_memory>\n");
            result.push_str(&agent_mem.content);

            let compacted_until = agent_mem
                .metadata
                .get("compacted_until")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<DateTime<Utc>>().ok());

            let new_insights = match compacted_until {
                Some(until) => {
                    self.insight_repo
                        .find_by_agent_id_after(agent_id, until)
                        .await?
                }
                None => self.insight_repo.find_by_agent_id(agent_id).await?,
            };
            if !new_insights.is_empty() {
                result.push('\n');
                for insight in &new_insights {
                    result.push_str(&format!("- {}\n", insight.content));
                }
            }

            result.push_str("</agent_memory>");
        } else {
            let insights = self.insight_repo.find_by_agent_id(agent_id).await?;
            tracing::debug!(
                agent_id = %agent_id,
                insight_count = insights.len(),
                "No compacted agent memory, using raw insights"
            );
            if !insights.is_empty() {
                result.push_str("\n\n<agent_memory>\n");
                for insight in &insights {
                    result.push_str(&format!("- {}\n", insight.content));
                }
                result.push_str("</agent_memory>");
            }
        }

        Ok(result)
    }
}
