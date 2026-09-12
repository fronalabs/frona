//! Compresses a long conversation so it fits the model's context window:
//! summarize the oldest messages into a rolling `chat_summary` row and load
//! only the messages after the cutoff (`created_at > compacted_until`) plus the
//! summary. Compacted messages are **retained**, not deleted. When a request
//! still can't fit after compaction we do NOT truncate - the provider rejects
//! it (fail loud).
//!
//! Summarization goes through the [`ChatSummarizer`] seam so tests can inject a
//! stub with no live model.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rig_core::completion::Message as RigMessage;

use crate::agent::prompt::PromptLoader;
use crate::chat::message::models::{Message, MessageRole};
use crate::chat::message::repository::MessageRepository;
use crate::chat::models::ChatSummary;
use crate::chat::repository::ChatSummaryRepository;
use crate::core::error::AppError;
use crate::core::repository::{Repository, new_id};
use crate::db::repo::chat_summaries::SurrealChatSummaryRepo;
use crate::db::repo::messages::SurrealMessageRepo;
use crate::inference::context::{estimate_message_tokens, estimate_tokens};
use crate::inference::conversation::{convert_agent_message, format_files_block_simple};
use crate::inference::provider::service::ModelProviderService;
use crate::inference::text_inference;
use crate::inference::usage::{CompactionTarget, InferenceKind, UsageContext, UsageService};

/// Trigger when the conversation exceeds this fraction of the available window.
const COMPACT_TRIGGER_PCT: usize = 80;
/// Compact down to roughly this fraction (leaves headroom for new turns).
const COMPACT_TARGET_PCT: usize = 70;

/// Result of a compaction-aware load: the rolling summary (if any) plus the
/// messages after the cutoff.
pub struct LoadedConversation {
    pub summary: Option<String>,
    pub messages: Vec<Message>,
}

/// Result of `compact_chat`: whether a new summary was written this call, plus the
/// live conversation (summary + post-cutoff messages) - built from data already in
/// hand, so callers needn't re-`load_conversation` and re-read the history.
pub struct CompactionOutcome {
    pub compacted: bool,
    pub conversation: LoadedConversation,
}

/// The summarization seam. Production wraps `text_inference` (which already
/// retries + falls back); tests inject a stub.
#[async_trait]
pub trait ChatSummarizer: Send + Sync {
    async fn summarize(
        &self,
        user_id: &str,
        agent_id: &str,
        chat_id: &str,
        system_prompt: &str,
        input: &str,
    ) -> Result<String, AppError>;
}

/// Production summarizer: resolves the `compaction` (→`primary`) model group and
/// calls `text_inference`, which carries its own retry/fallback. On exhausted
/// retries it returns `Err`, which `compact_chat` propagates (fail loud).
#[derive(Clone)]
pub struct TextInferenceSummarizer {
    model_providers: ModelProviderService,
    usage_service: UsageService,
}

impl TextInferenceSummarizer {
    pub fn new(model_providers: ModelProviderService, usage_service: UsageService) -> Self {
        Self {
            model_providers,
            usage_service,
        }
    }
}

#[async_trait]
impl ChatSummarizer for TextInferenceSummarizer {
    async fn summarize(
        &self,
        user_id: &str,
        agent_id: &str,
        chat_id: &str,
        system_prompt: &str,
        input: &str,
    ) -> Result<String, AppError> {
        let model_group = self
            .model_providers
            .resolve_with_fallback(
                &crate::inference::ModelRef::COMPACTION,
                &crate::inference::ModelRef::PRIMARY,
            )
            .map_err(|e| AppError::Internal(format!("No compaction model group: {e}")))?;
        let usage_ctx = UsageContext::new(
            InferenceKind::Compaction {
                target: CompactionTarget::Chat {
                    agent_id: agent_id.to_string(),
                    chat_id: chat_id.to_string(),
                },
            },
            user_id,
            model_group.name.clone(),
        );
        text_inference(
            &model_group,
            system_prompt,
            vec![RigMessage::user(input)],
            &self.usage_service,
            &usage_ctx,
        )
        .await
        .map_err(|e| AppError::Internal(format!("Chat compaction failed: {e}")))
    }
}

#[derive(Clone)]
pub struct ChatCompactor {
    chat_summary_repo: SurrealChatSummaryRepo,
    message_repo: SurrealMessageRepo,
    summarizer: Arc<dyn ChatSummarizer>,
    prompts: PromptLoader,
}

impl ChatCompactor {
    pub fn new(
        chat_summary_repo: SurrealChatSummaryRepo,
        message_repo: SurrealMessageRepo,
        summarizer: Arc<dyn ChatSummarizer>,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            chat_summary_repo,
            message_repo,
            summarizer,
            prompts,
        }
    }

    /// Compaction-aware load: the rolling summary + the messages after the
    /// cutoff. Used by the live-context builders (Path A + Path B).
    pub async fn load_conversation(&self, chat_id: &str) -> Result<LoadedConversation, AppError> {
        let existing = self.chat_summary_repo.find_by_chat_id(chat_id).await?;
        match existing {
            Some(summary) => {
                // Fetch only post-cutoff messages in SQL, not the whole history.
                let messages = self
                    .message_repo
                    .find_by_chat_id_after(chat_id, summary.compacted_until)
                    .await?;
                Ok(LoadedConversation {
                    summary: Some(summary.content),
                    messages,
                })
            }
            None => Ok(LoadedConversation {
                summary: None,
                messages: self.message_repo.find_by_chat_id(chat_id).await?,
            }),
        }
    }

    /// Summarize the oldest not-yet-compacted messages if the conversation
    /// exceeds the trigger threshold. No-op (returns `false`) when under
    /// threshold. Returns `true` when a new/updated summary was written.
    /// Retains compacted messages (does not delete them).
    pub async fn compact_chat(
        &self,
        user_id: &str,
        chat_id: &str,
        chat_agent_id: &str,
        system_prompt: &str,
        context_window: usize,
        max_output_tokens: usize,
    ) -> Result<CompactionOutcome, AppError> {
        let existing = self.chat_summary_repo.find_by_chat_id(chat_id).await?;
        // Only the messages NOT already covered by the summary are live. Fetch just
        // those in SQL - the compacted rest is retained on disk but represented by
        // the summary, so we never re-read (or re-compact) it each turn.
        let mut live: Vec<Message> = match &existing {
            Some(s) => {
                self.message_repo
                    .find_by_chat_id_after(chat_id, s.compacted_until)
                    .await?
            }
            None => self.message_repo.find_by_chat_id(chat_id).await?,
        };
        let existing_summary = existing.as_ref().map(|s| s.content.clone());
        let unchanged = |summary, messages| CompactionOutcome {
            compacted: false,
            conversation: LoadedConversation { summary, messages },
        };
        if live.is_empty() {
            return Ok(unchanged(existing_summary, live));
        }

        let rig_messages = self.to_rig(&live, chat_agent_id);
        let available = context_window.saturating_sub(max_output_tokens);

        let mut total_tokens = estimate_tokens(system_prompt);
        if let Some(ref s) = existing {
            total_tokens += estimate_tokens(&s.content);
        }
        for msg in &rig_messages {
            total_tokens += estimate_message_tokens(msg);
        }
        if total_tokens <= available * COMPACT_TRIGGER_PCT / 100 {
            return Ok(unchanged(existing_summary, live));
        }

        // Walk newest→oldest keeping a tail under the target; everything older
        // becomes the to-compact prefix.
        let target = available * COMPACT_TARGET_PCT / 100;
        let mut summary_budget = estimate_tokens(system_prompt);
        if let Some(ref s) = existing {
            summary_budget += estimate_tokens(&s.content);
        }
        let mut keep_from_idx = live.len();
        let mut running = 0usize;
        for (i, msg) in rig_messages.iter().enumerate().rev() {
            let cost = estimate_message_tokens(msg);
            if running + cost + summary_budget > target {
                break;
            }
            running += cost;
            keep_from_idx = i;
        }
        if keep_from_idx == 0 {
            return Ok(unchanged(existing_summary, live));
        }
        let messages_to_compact = &live[..keep_from_idx];

        let mut input = String::new();
        if let Some(ref s) = existing {
            input.push_str("Previous summary:\n");
            input.push_str(&s.content);
            input.push_str("\n\nNew messages to incorporate:\n");
        }
        for msg in messages_to_compact {
            let role = match msg.role {
                MessageRole::User => "User",
                MessageRole::Agent => "Agent",
                MessageRole::TaskCompletion => "System",
                MessageRole::Contact => "Contact",
                MessageRole::LiveCall => "Caller",
                MessageRole::System => continue,
            };
            input.push_str(&format!("{role}: {}\n", msg.content));
        }

        let prompt = self.prompts.read("CHAT_COMPACTION.md").ok_or_else(|| {
            AppError::Internal("CHAT_COMPACTION.md prompt missing from resources".into())
        })?;
        let content = self
            .summarizer
            .summarize(user_id, chat_agent_id, chat_id, &prompt, &input)
            .await?;

        let now = Utc::now();
        let compacted_until = messages_to_compact
            .last()
            .map(|m| m.created_at)
            .unwrap_or(now);
        let item_count = messages_to_compact.len() as i64;
        let row = ChatSummary {
            id: existing
                .as_ref()
                .map(|s| s.id.clone())
                .unwrap_or_else(new_id),
            user_id: user_id.to_string(),
            chat_id: chat_id.to_string(),
            content: content.clone(),
            compacted_until,
            item_count,
            created_at: existing.as_ref().map(|s| s.created_at).unwrap_or(now),
            updated_at: now,
        };
        if existing.is_some() {
            self.chat_summary_repo.update(&row).await?;
        } else {
            self.chat_summary_repo.create(&row).await?;
        }
        let messages = live.split_off(keep_from_idx);
        Ok(CompactionOutcome {
            compacted: true,
            conversation: LoadedConversation {
                summary: Some(content),
                messages,
            },
        })
    }

    /// Approximate rig messages for token *estimation* only - coarser than the
    /// conversation builder (roles grouped, tool calls omitted). `System` rows drop.
    fn to_rig(&self, messages: &[Message], chat_agent_id: &str) -> Vec<RigMessage> {
        messages
            .iter()
            .filter_map(|msg| match msg.role {
                MessageRole::User | MessageRole::TaskCompletion | MessageRole::Contact => {
                    let content = format_files_block_simple(&msg.content, &msg.attachments);
                    Some(RigMessage::user(&content))
                }
                MessageRole::LiveCall => {
                    let content = format_files_block_simple(&msg.content, &msg.attachments);
                    Some(RigMessage::user(format!("[LIVE_CALL] {content}")))
                }
                MessageRole::Agent => convert_agent_message(msg, chat_agent_id, None),
                MessageRole::System => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use chrono::Duration;
    use surrealdb::Surreal;
    use surrealdb::engine::local::Mem;

    use crate::chat::repository::ChatSummaryRepository;
    use crate::db::repo::chat_summaries::SurrealChatSummaryRepo;
    use crate::db::repo::generic::SurrealRepo;
    use crate::db::repo::messages::SurrealMessageRepo;

    /// Returns `[SUMMARY]`, or errors when `fail` - no live model.
    struct StubSummarizer {
        fail: bool,
    }

    #[async_trait]
    impl ChatSummarizer for StubSummarizer {
        async fn summarize(
            &self,
            _user_id: &str,
            _agent_id: &str,
            _chat_id: &str,
            _system_prompt: &str,
            _input: &str,
        ) -> Result<String, AppError> {
            if self.fail {
                Err(AppError::Internal("stub summarizer failure".into()))
            } else {
                Ok("[SUMMARY]".into())
            }
        }
    }

    fn prompts() -> PromptLoader {
        PromptLoader::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("resources")
                .join("prompts"),
        )
    }

    async fn setup(fail: bool) -> (ChatCompactor, SurrealMessageRepo, SurrealChatSummaryRepo) {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let message_repo: SurrealMessageRepo = SurrealRepo::new(db.clone());
        let summary_repo: SurrealChatSummaryRepo = SurrealRepo::new(db.clone());
        let compactor = ChatCompactor::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            Arc::new(StubSummarizer { fail }),
            prompts(),
        );
        (compactor, message_repo, summary_repo)
    }

    async fn seed(repo: &SurrealMessageRepo, chat_id: &str, n: usize, len: usize) {
        let base = Utc::now() - Duration::hours(1);
        for i in 0..n {
            let mut m = Message::builder(chat_id, MessageRole::User, "x".repeat(len)).build();
            m.id = new_id();
            m.created_at = base + Duration::seconds(i as i64);
            repo.create(&m).await.unwrap();
        }
    }

    #[tokio::test]
    async fn compacts_persists_retains_and_loads_cutoff() {
        let (compactor, messages, summaries) = setup(false).await;
        let chat = "chat-1";
        seed(&messages, chat, 6, 200).await;

        let out = compactor
            .compact_chat("u", chat, "agent", "sys", 200, 0)
            .await
            .unwrap();
        assert!(out.compacted, "expected compaction to run");

        let summary = summaries.find_by_chat_id(chat).await.unwrap().unwrap();
        assert_eq!(summary.content, "[SUMMARY]");
        assert!(summary.item_count > 0 && summary.item_count < 6);

        let all = messages.find_by_chat_id(chat).await.unwrap();
        assert_eq!(all.len(), 6, "compacted messages must be retained");

        let loaded = compactor.load_conversation(chat).await.unwrap();
        assert_eq!(loaded.summary.as_deref(), Some("[SUMMARY]"));
        assert_eq!(loaded.messages.len(), 6 - summary.item_count as usize);
        assert!(
            loaded
                .messages
                .iter()
                .all(|m| m.created_at > summary.compacted_until)
        );

        assert_eq!(out.conversation.summary, loaded.summary);
        assert_eq!(
            out.conversation
                .messages
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>(),
            loaded
                .messages
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn no_op_under_threshold() {
        let (compactor, messages, summaries) = setup(false).await;
        let chat = "chat-2";
        seed(&messages, chat, 1, 20).await;

        let out = compactor
            .compact_chat("u", chat, "agent", "sys", 100_000, 0)
            .await
            .unwrap();
        assert!(!out.compacted);
        assert!(summaries.find_by_chat_id(chat).await.unwrap().is_none());
        assert!(out.conversation.summary.is_none());
        assert_eq!(out.conversation.messages.len(), 1);

        let loaded = compactor.load_conversation(chat).await.unwrap();
        assert!(loaded.summary.is_none());
        assert_eq!(loaded.messages.len(), 1);
    }

    #[tokio::test]
    async fn prior_summary_returns_summary_and_post_cutoff() {
        let (compactor, messages, summaries) = setup(false).await;
        let chat = "chat-4";
        seed(&messages, chat, 6, 200).await;

        let first = compactor
            .compact_chat("u", chat, "agent", "sys", 200, 0)
            .await
            .unwrap();
        assert!(first.compacted);
        let summary = summaries.find_by_chat_id(chat).await.unwrap().unwrap();

        let second = compactor
            .compact_chat("u", chat, "agent", "sys", 100_000, 0)
            .await
            .unwrap();
        assert!(!second.compacted, "must not re-compact under threshold");
        assert_eq!(second.conversation.summary.as_deref(), Some("[SUMMARY]"));
        assert_eq!(
            second.conversation.messages.len(),
            6 - summary.item_count as usize
        );
        assert!(
            second
                .conversation
                .messages
                .iter()
                .all(|m| m.created_at > summary.compacted_until)
        );
    }

    #[tokio::test]
    async fn fail_loud_on_summarizer_error() {
        let (compactor, messages, summaries) = setup(true).await;
        let chat = "chat-3";
        seed(&messages, chat, 6, 200).await;

        let err = compactor
            .compact_chat("u", chat, "agent", "sys", 200, 0)
            .await;
        assert!(err.is_err(), "summarizer failure must propagate");
        assert!(
            summaries.find_by_chat_id(chat).await.unwrap().is_none(),
            "no partial summary on failure"
        );
    }
}
