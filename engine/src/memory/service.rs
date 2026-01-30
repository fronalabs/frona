use std::sync::Arc;

use chrono::{DateTime, Utc};
use rig::completion::Message as RigMessage;

use crate::api::repo::facts::SurrealFactRepo;
use crate::api::repo::memories::SurrealMemoryRepo;
use crate::api::repo::messages::SurrealMessageRepo;
use crate::chat::message::models::Message;
use crate::chat::message::repository::MessageRepository;
use crate::error::AppError;
use crate::llm::config::ModelGroup;
use crate::llm::context::{estimate_tokens, resolve_context_window};
use crate::llm::convert::to_rig_messages;
use crate::llm::fallback::inference_with_fallback;
use crate::llm::ModelProviderRegistry;
use crate::memory::fact::models::Fact;
use crate::memory::fact::repository::FactRepository;
use crate::memory::models::{Memory, MemorySourceType};
use crate::memory::repository::MemoryRepository;
use crate::repository::Repository;

const FACT_COMPACTION_TOKEN_THRESHOLD: usize = 3_000;

const CHAT_COMPACTION_PROMPT: &str = "\
You are a conversation summarizer. Summarize the following conversation into a concise summary \
that preserves all important context, decisions, facts, and any information that would be needed \
to continue the conversation naturally. Include key topics discussed, conclusions reached, \
and any action items or pending questions. Be thorough but concise.";

const FACT_COMPACTION_PROMPT: &str = "\
You are a strict knowledge compactor. You receive facts previously stored about a user by an AI agent. \
Your job is to produce a clean, deduplicated bullet-point list of user-centric facts.\n\
\n\
Rules:\n\
- Output ONLY bullet points (lines starting with '- '). No headers, prose, or commentary.\n\
- Each bullet = one atomic fact about the USER (preferences, personal info, project details, decisions).\n\
- Aggressively deduplicate: if two bullets say the same thing in different words, keep the most recent/specific one.\n\
- Resolve contradictions by keeping the most recent information and dropping the outdated version.\n\
- DELETE junk: remove assistant responses stored as facts, generic observations, trivial conversation artifacts, \
  and anything that is not a concrete fact about the user.\n\
- Keep the list as short as possible while preserving all genuinely useful information.";

const SPACE_COMPACTION_PROMPT: &str = "\
You are a workspace summarizer. Summarize the following collection of chat summaries from a shared workspace. \
Create a high-level overview of the workspace context including: main topics and projects discussed, \
key decisions made, important context that would help in new conversations within this workspace.";

#[derive(Clone)]
pub struct MemoryService {
    memory_repo: SurrealMemoryRepo,
    fact_repo: SurrealFactRepo,
    message_repo: SurrealMessageRepo,
    provider_registry: Arc<ModelProviderRegistry>,
}

impl MemoryService {
    pub fn new(
        memory_repo: SurrealMemoryRepo,
        fact_repo: SurrealFactRepo,
        message_repo: SurrealMessageRepo,
        provider_registry: Arc<ModelProviderRegistry>,
    ) -> Self {
        Self {
            memory_repo,
            fact_repo,
            message_repo,
            provider_registry,
        }
    }

    pub async fn compact_chat_if_needed(
        &self,
        chat_id: &str,
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

        let rig_messages = to_rig_messages(&messages);
        let window = resolve_context_window(model_id, context_window);
        let available = window.saturating_sub(max_output_tokens);

        let mut total_tokens = estimate_tokens(system_prompt);
        for msg in &rig_messages {
            total_tokens += crate::llm::context::estimate_message_tokens(msg);
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
            let cost = crate::llm::context::estimate_message_tokens(msg);
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
                crate::chat::message::models::MessageRole::Assistant => "Assistant",
                crate::chat::message::models::MessageRole::ToolResult => "Tool",
            };
            compaction_input.push_str(&format!("{role_str}: {}\n", msg.content));
        }

        let user_msg = RigMessage::user(&compaction_input);
        let summary = inference_with_fallback(
            &self.provider_registry,
            compaction_model_group,
            CHAT_COMPACTION_PROMPT,
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

    pub async fn store_fact(
        &self,
        agent_id: &str,
        content: &str,
        source_chat_id: Option<&str>,
    ) -> Result<Fact, AppError> {
        tracing::debug!(agent_id = %agent_id, fact = %content, "Storing fact");

        let fact = Fact {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            content: content.to_string(),
            source_chat_id: source_chat_id.map(|s| s.to_string()),
            created_at: Utc::now(),
        };

        self.fact_repo.create(&fact).await
    }

    pub async fn compact_facts_if_needed(
        &self,
        agent_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let facts = self.fact_repo.find_by_agent_id(agent_id).await?;
        let total_tokens: usize = facts.iter().map(|f| estimate_tokens(&f.content)).sum();

        if total_tokens <= FACT_COMPACTION_TOKEN_THRESHOLD {
            tracing::debug!(
                agent_id = %agent_id,
                token_count = total_tokens,
                threshold = FACT_COMPACTION_TOKEN_THRESHOLD,
                "Skipping fact compaction (below threshold)"
            );
            return Ok(());
        }

        self.compact_facts(agent_id, facts, compaction_model_group).await
    }

    pub async fn compact_facts_forced(
        &self,
        agent_id: &str,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let facts = self.fact_repo.find_by_agent_id(agent_id).await?;
        if facts.is_empty() {
            return Ok(());
        }
        self.compact_facts(agent_id, facts, compaction_model_group).await
    }

    async fn compact_facts(
        &self,
        agent_id: &str,
        facts: Vec<Fact>,
        compaction_model_group: &ModelGroup,
    ) -> Result<(), AppError> {
        let token_count_before: usize = facts.iter().map(|f| estimate_tokens(&f.content)).sum();
        tracing::info!(
            agent_id = %agent_id,
            fact_count = facts.len(),
            token_count = token_count_before,
            "Running fact compaction"
        );

        let existing_memory = self
            .memory_repo
            .find_latest(MemorySourceType::Agent, agent_id)
            .await?;

        let mut compaction_input = String::new();
        if let Some(ref mem) = existing_memory {
            compaction_input.push_str("Previous agent memory:\n");
            compaction_input.push_str(&mem.content);
            compaction_input.push_str("\n\nNew facts to incorporate:\n");
        }
        for fact in &facts {
            compaction_input.push_str(&format!("- {}\n", fact.content));
        }

        let user_msg = RigMessage::user(&compaction_input);
        let summary = inference_with_fallback(
            &self.provider_registry,
            compaction_model_group,
            FACT_COMPACTION_PROMPT,
            vec![],
            user_msg,
        )
        .await
        .map_err(|e| AppError::Internal(format!("Fact compaction failed: {e}")))?;

        let token_count_after = estimate_tokens(&summary);
        tracing::info!(
            agent_id = %agent_id,
            token_count_before,
            token_count_after,
            "Fact compaction complete"
        );

        let now = Utc::now();
        let last_fact_time = facts.last().map(|f| f.created_at).unwrap_or(now);

        let memory = Memory {
            id: existing_memory
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            source_type: MemorySourceType::Agent,
            source_id: agent_id.to_string(),
            content: summary,
            metadata: serde_json::json!({
                "compacted_until": last_fact_time,
                "item_count": facts.len(),
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

        self.fact_repo
            .delete_by_agent_id_before(agent_id, last_fact_time)
            .await?;

        Ok(())
    }

    pub async fn compact_space(
        &self,
        space_id: &str,
        chat_summaries: Vec<(String, String)>, // (chat_title, summary_or_messages)
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
        let summary = inference_with_fallback(
            &self.provider_registry,
            compaction_model_group,
            SPACE_COMPACTION_PROMPT,
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

    pub async fn build_augmented_system_prompt(
        &self,
        base_prompt: &str,
        agent_id: &str,
        space_id: Option<&str>,
        skill_summaries: &[(String, String)],
    ) -> Result<String, AppError> {
        let mut prefix = String::new();

        if let Some(sid) = space_id {
            if let Some(space_mem) = self
                .get_memory(MemorySourceType::Space, sid)
                .await?
            {
                prefix.push_str("<space_context>\n");
                prefix.push_str(&space_mem.content);
                prefix.push_str("\n</space_context>\n\n");
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
            prefix.push_str("<agent_memory>\n");
            prefix.push_str(&agent_mem.content);

            let compacted_until = agent_mem
                .metadata
                .get("compacted_until")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<DateTime<Utc>>().ok());

            let new_facts = match compacted_until {
                Some(until) => {
                    self.fact_repo
                        .find_by_agent_id_after(agent_id, until)
                        .await?
                }
                None => self.fact_repo.find_by_agent_id(agent_id).await?,
            };
            if !new_facts.is_empty() {
                prefix.push('\n');
                for fact in &new_facts {
                    prefix.push_str(&format!("- {}\n", fact.content));
                }
            }

            prefix.push_str("</agent_memory>\n\n");
        } else {
            let facts = self.fact_repo.find_by_agent_id(agent_id).await?;
            tracing::debug!(
                agent_id = %agent_id,
                fact_count = facts.len(),
                "No compacted agent memory, using raw facts"
            );
            if !facts.is_empty() {
                prefix.push_str("<agent_memory>\n");
                for fact in &facts {
                    prefix.push_str(&format!("- {}\n", fact.content));
                }
                prefix.push_str("</agent_memory>\n\n");
            }
        }

        if !skill_summaries.is_empty() {
            prefix.push_str("<available_skills>\nThe following skills contain instructions and knowledge you can load using the `read_skill` tool when relevant to the conversation. Use skills transparently — do not tell the user you are loading or using a skill. Just follow the skill's instructions naturally.\n");
            for (name, description) in skill_summaries {
                prefix.push_str(&format!("- {name}: {description}\n"));
            }
            prefix.push_str("</available_skills>\n\n");
        }

        if prefix.is_empty() {
            Ok(base_prompt.to_string())
        } else {
            Ok(format!("{prefix}---\n{base_prompt}"))
        }
    }
}
