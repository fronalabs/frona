//! Compresses a long conversation so it fits the model's context window:
//! summarize the oldest messages into a rolling `chat_summary` row and load
//! only the messages after the cutoff (`created_at > compacted_until`) plus the
//! summary. Compacted messages are **retained**, not deleted. When a request
//! still cannot fit after compaction we return a limit error and preserve its
//! persisted rows. Initial model context is also checked before inference trimming.
//!
//! Summarization goes through the [`ChatSummarizer`] seam so tests can inject a
//! stub with no live model.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};

use crate::agent::prompt::PromptLoader;
use crate::chat::message::models::{Message, MessageCommand, MessageRole};
use crate::chat::message::repository::MessageRepository;
use crate::chat::models::ChatSummary;
use crate::chat::repository::ChatSummaryRepository;
use crate::core::error::AppError;
use crate::core::repository::{Repository, new_id};
use crate::db::repo::chat_summaries::SurrealChatSummaryRepo;
use crate::db::repo::messages::SurrealMessageRepo;
use crate::db::repo::tool_calls::ToolCallRepository;
use crate::inference::ModelGroup;
use crate::inference::context::{InputTokenEstimate, estimate_message_tokens, estimate_tokens};
use crate::inference::conversation::{ModelConversationHistory, summary_message};
use crate::inference::usage::repository::InferenceUsageRepository;
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

/// Compaction status and the resulting model-facing history.
pub struct CompactionOutcome {
    pub compacted: bool,
    pub model_history: ModelConversationHistory,
    pub input_token_estimate: Option<InputTokenEstimate>,
}

pub struct SummaryRequest<'a> {
    pub input_token_estimate: Option<InputTokenEstimate>,
    pub model_group: &'a ModelGroup,
    pub user_id: &'a str,
    pub agent_id: &'a str,
    pub chat_id: &'a str,
    pub system_prompt: &'a str,
    pub history: Vec<RigMessage>,
    pub tools: Vec<RigToolDefinition>,
}

/// The exact model-facing request prefix; tests can inject deterministic generation.
#[async_trait]
pub trait ChatSummarizer: Send + Sync {
    async fn summarize(&self, request: SummaryRequest<'_>) -> Result<String, AppError>;
}

#[derive(Clone, Copy)]
pub struct CompactionContext<'a> {
    pub system_prompt: &'a str,
    pub model_group: &'a ModelGroup,
    pub model_history: &'a ModelConversationHistory,
    pub tools: &'a [RigToolDefinition],
}

/// Generates a summary with the conversation's resolved model group, preserving
/// its request settings, retry/fallback configuration and usage accounting.
/// The selected input prefix is never truncated.
#[derive(Clone)]
pub struct TextInferenceSummarizer {
    usage_service: UsageService,
}

impl TextInferenceSummarizer {
    pub fn new(usage_service: UsageService) -> Self {
        Self { usage_service }
    }
}

#[async_trait]
impl ChatSummarizer for TextInferenceSummarizer {
    async fn summarize(&self, request: SummaryRequest<'_>) -> Result<String, AppError> {
        let model_group = request.model_group;
        let usage_ctx = UsageContext::new(
            InferenceKind::Compaction {
                target: CompactionTarget::Chat {
                    agent_id: request.agent_id.to_string(),
                    chat_id: request.chat_id.to_string(),
                },
            },
            request.user_id,
            model_group.name.clone(),
        );
        let response = model_group
            .inference_preserving_history(crate::inference::ModelRequest {
                input_token_estimate: request.input_token_estimate,
                system_prompt: request.system_prompt,
                history: request.history,
                tools: request.tools,
                usage_service: &self.usage_service,
                usage_context: &usage_ctx,
                overrides: Default::default(),
            })
            .await
            .map_err(|e| AppError::Internal(format!("Chat compaction failed: {e}")))?;
        // This is generation only: definitions preserve the cached prefix, never
        // authorize tool execution. A tool request is not a completed summary.
        if response
            .content
            .iter()
            .any(|item| matches!(item, AssistantContent::ToolCall(_)))
        {
            return Err(AppError::Internal(
                "Chat compaction returned tool calls instead of a summary".into(),
            ));
        }
        let text = response
            .content
            .iter()
            .filter_map(|item| match item {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            return Err(AppError::Internal(
                "Chat compaction returned an empty summary".into(),
            ));
        }
        Ok(text)
    }
}

struct HistoryTokenMeasurement {
    /// Inclusive index of the last stored row covered by the provider usage.
    last_covered_row: usize,
    history_tokens: usize,
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

    /// Usage is optional telemetry. Missing, stale or unreadable statistics must
    /// leave both the trigger and split on their existing estimate-based path.
    async fn load_history_token_measurement(
        &self,
        user_id: &str,
        agent_id: &str,
        live: &[Message],
        context: CompactionContext<'_>,
        summary: Option<&ChatSummary>,
        system_tokens: usize,
    ) -> Option<HistoryTokenMeasurement> {
        let repo = crate::db::repo::generic::SurrealRepo::new(self.message_repo.db().clone());
        let usage = match repo.last_chat_usage(&live.first()?.chat_id).await {
            Ok(usage) => usage?,
            Err(error) => {
                tracing::warn!(%error, "Cannot read chat usage; estimating compaction size");
                return None;
            }
        };
        if usage.user_id != user_id
            || usage.agent_id.as_deref() != Some(agent_id)
            || usage.model_group != context.model_group.name
            || usage.model_ref != context.model_group.main.as_str()
            || usage.provider != context.model_group.main.provider_name()
            || summary.is_some_and(|s| usage.created_at <= s.updated_at)
        {
            return None;
        }
        let anchor = live
            .iter()
            .position(|message| Some(&message.id) == usage.message_id.as_ref())?;
        let message = &live[anchor];
        if message.role != MessageRole::Agent
            || message.agent_id.as_deref() != Some(agent_id)
            || message.status != Some(crate::chat::message::models::MessageStatus::Completed)
        {
            return None;
        }
        let tools =
            crate::db::repo::tool_calls::SurrealToolCallRepo::new(self.message_repo.db().clone())
                .find_by_message_id(&message.id)
                .await
                .ok()?;
        let turns = tools
            .iter()
            .map(|tool| tool.turn)
            .collect::<std::collections::BTreeSet<_>>();
        let complete_tool_history = turns.iter().copied().eq(0..turns.len() as u32);
        match usage.kind_tag.as_str() {
            "Text" if tools.is_empty() && usage.turn_index.is_none() => {}
            "ToolTurn"
                if complete_tool_history && usage.turn_index == u32::try_from(turns.len()).ok() => {
            }
            _ => return None,
        }
        if usage.output_tokens == 0 && (!message.content.is_empty() || message.reasoning.is_some())
        {
            return None;
        }
        // Rig Anthropic input excludes cache reads/writes; total includes both.
        // OpenAI input already includes cached reads. Never add caches to total.
        // Reject incomplete/inconsistent usage instead of guessing provider rules.
        let input = usage.total_tokens.checked_sub(usage.output_tokens)?;
        let minimum_input = if crate::inference::protocol::parameters::WireDialect::for_model(
            &context.model_group.main.provider,
        ) == crate::inference::protocol::parameters::WireDialect::Anthropic
        {
            usage.input_tokens.checked_add(usage.cached_input_tokens)?
        } else {
            usage.input_tokens.max(usage.cached_input_tokens)
        };
        if input == 0 || input < minimum_input {
            return None;
        }
        let total = usize::try_from(usage.total_tokens).ok()?;
        // A call larger than this model's entire context is not a compatible
        // measurement (including after a configured window reduction).
        if total > context.model_group.context_window {
            return None;
        }
        let history = total
            .checked_sub(system_tokens)
            .filter(|tokens| *tokens > 0)?;
        Some(HistoryTokenMeasurement {
            last_covered_row: anchor,
            history_tokens: history,
        })
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
        context: CompactionContext<'_>,
    ) -> Result<CompactionOutcome, AppError> {
        let CompactionContext {
            system_prompt,
            model_group,
            model_history,
            tools,
        } = context;
        let context_window = model_group.context_window;
        let max_output_tokens = model_group
            .main
            .request_settings
            .max_tokens
            .unwrap_or(model_group.inference.default_max_tokens)
            as usize;
        let existing = self.chat_summary_repo.find_by_chat_id(chat_id).await?;
        // Only the messages NOT already covered by the summary are live. Fetch just
        // those in SQL - the compacted rest is retained on disk but represented by
        // the summary, so we never re-read (or re-compact) it each turn.
        let live: Vec<Message> = match &existing {
            Some(s) => {
                self.message_repo
                    .find_by_chat_id_after(chat_id, s.compacted_until)
                    .await?
            }
            None => self.message_repo.find_by_chat_id(chat_id).await?,
        };
        let unchanged = |input_token_estimate| CompactionOutcome {
            compacted: false,
            model_history: model_history.clone(),
            input_token_estimate,
        };
        if live.is_empty() {
            return Ok(unchanged(None));
        }

        // These offsets came from the normal conversation builder. Validate the
        // source before using them as a database cutoff; a stale/reordered request
        // must never summarize a different set of persisted rows.
        model_history.validate_source(
            &live,
            existing.as_ref().map(|summary| summary.content.as_str()),
        )?;
        let mut costs: Vec<usize> = (0..live.len())
            .map(|row| {
                model_history.history[model_history.row_range(row)]
                    .iter()
                    .map(estimate_message_tokens)
                    .sum()
            })
            .collect();
        // Builder-injected continuation/heartbeat messages have no database row.
        // They remain protected with the tail and never enter the summarized prefix.
        let stored_end = model_history.row_range(live.len() - 1).end;
        let injected_tokens = model_history.history[stored_end..]
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>();
        let available = context_window.saturating_sub(max_output_tokens);
        let system_tokens = estimate_tokens(system_prompt)
            + tools
                .iter()
                .map(|tool| {
                    estimate_tokens(
                        &serde_json::to_string(tool).expect("tool definition is serializable"),
                    )
                })
                .sum::<usize>();
        let mut summary_tokens = model_history.history[..model_history.summary_end]
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>();
        // Calibrate only the prefix actually covered by the last completed call.
        // Its input already includes prior tool turns; adding the whole owning
        // assistant row again would double-count them. New rows stay estimated.
        let calibrated = if let Some(HistoryTokenMeasurement {
            last_covered_row,
            history_tokens,
        }) = self
            .load_history_token_measurement(
                user_id,
                chat_agent_id,
                &live,
                context,
                existing.as_ref(),
                system_tokens,
            )
            .await
        {
            let estimated_history =
                summary_tokens + costs[..=last_covered_row].iter().sum::<usize>();
            let fits_counter = history_tokens
                .checked_add(system_tokens)
                .and_then(|total| {
                    total.checked_add(costs[last_covered_row + 1..].iter().sum::<usize>())
                })
                .and_then(|total| total.checked_add(injected_tokens))
                .and_then(|total| total.checked_add(costs.len() + 1))
                .is_some();
            if estimated_history > 0 && fits_counter {
                let scale = |tokens: usize| -> usize {
                    ((tokens as u128 * history_tokens as u128).div_ceil(estimated_history as u128))
                        .min(usize::MAX as u128) as usize
                };
                summary_tokens = scale(summary_tokens);
                for cost in &mut costs[..=last_covered_row] {
                    *cost = scale(*cost);
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        if let Some(last) = costs.last_mut() {
            *last += injected_tokens;
        }
        let total_tokens = system_tokens + summary_tokens + costs.iter().sum::<usize>();
        let input_token_estimate = calibrated.then(|| {
            InputTokenEstimate::new(
                total_tokens,
                &model_group.main,
                system_prompt,
                &model_history.history,
                tools,
            )
        });
        if total_tokens <= available * COMPACT_TRIGGER_PCT / 100 {
            return Ok(unchanged(input_token_estimate));
        }

        // An empty assistant placeholder (or a slash-command acknowledgement)
        // must never move the protection boundary past the actual request.
        let protected = live
            .iter()
            .rposition(is_substantive_request)
            .unwrap_or(live.len() - 1);
        let protected = timestamp_group_start(&live, protected);
        let protected_tokens = costs[protected..].iter().sum::<usize>();
        ensure_fits(system_tokens + protected_tokens, available)?;

        let target = available * COMPACT_TARGET_PCT / 100;
        let mut keep_from_idx = protected;
        let mut running = protected_tokens;
        for i in (0..protected).rev() {
            if running + costs[i] + system_tokens + summary_tokens > target {
                break;
            }
            running += costs[i];
            keep_from_idx = i;
        }
        // The existing persisted cutoff is timestamp-only and exclusive when
        // loading. Keep the entire boundary timestamp group so reload cannot
        // silently discard a retained row with the same timestamp as the prefix.
        keep_from_idx = timestamp_group_start(&live, keep_from_idx);
        if keep_from_idx == 0 {
            ensure_fits(total_tokens, available)?;
            return Ok(unchanged(input_token_estimate));
        }
        let messages_to_compact = &live[..keep_from_idx];

        let prefix_end = model_history.row_range(keep_from_idx - 1).end;
        let mut history = model_history.history[..prefix_end].to_vec();
        let instruction = self.prompts.read("CHAT_COMPACTION.md").ok_or_else(|| {
            AppError::Internal("CHAT_COMPACTION.md prompt missing from resources".into())
        })?;
        history.push(RigMessage::user(instruction));
        let summary_input_tokens = system_tokens
            + summary_tokens
            + costs[..keep_from_idx].iter().sum::<usize>()
            + estimate_message_tokens(history.last().expect("summary instruction"));
        let content = self
            .summarizer
            .summarize(SummaryRequest {
                input_token_estimate: calibrated.then(|| {
                    InputTokenEstimate::new(
                        summary_input_tokens,
                        &model_group.main,
                        system_prompt,
                        &history,
                        tools,
                    )
                }),
                model_group,
                user_id,
                agent_id: chat_agent_id,
                chat_id,
                system_prompt,
                history,
                tools: tools.to_vec(),
            })
            .await?;
        if content.trim().is_empty() {
            return Err(AppError::Internal(
                "Chat compaction returned an empty summary".into(),
            ));
        }

        // Do not publish a summary that still cannot fit alongside the retained
        // request. The original rows and prior summary remain recoverable on error.
        ensure_fits(
            system_tokens
                + summary_tokens_for(&content)
                + costs[keep_from_idx..].iter().sum::<usize>(),
            available,
        )?;
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
        let mut compacted_history = model_history.clone();
        compacted_history.replace_prefix(keep_from_idx, &content);
        let input_token_estimate = calibrated.then(|| {
            InputTokenEstimate::new(
                system_tokens
                    + summary_tokens_for(&content)
                    + costs[keep_from_idx..].iter().sum::<usize>(),
                &model_group.main,
                system_prompt,
                &compacted_history.history,
                tools,
            )
        });
        Ok(CompactionOutcome {
            input_token_estimate,
            compacted: true,
            model_history: compacted_history,
        })
    }
}

/// Validate the actual, fully built initial context against the budget used by
/// inference's truncation layer. Compaction estimates are deliberately coarse;
/// this final check prevents initial requests/attachments being silently trimmed.
/// This covers the initial main-model request. Fallback budgets and tool-loop
/// growth after this boundary are handled separately by inference.
pub(crate) fn validate_initial_context(
    history: &[RigMessage],
    system_prompt: &str,
    model_group: &crate::inference::ModelGroup,
    tools: &[RigToolDefinition],
    input_token_estimate: Option<InputTokenEstimate>,
) -> Result<(), AppError> {
    if let Some(tokens) = input_token_estimate
        .and_then(|estimate| estimate.for_request(&model_group.main, system_prompt, history, tools))
    {
        let output = model_group
            .main
            .request_settings
            .max_tokens
            .unwrap_or(model_group.inference.default_max_tokens) as usize;
        return ensure_fits(tokens, model_group.context_window.saturating_sub(output));
    }
    let tokens = history.iter().map(estimate_message_tokens).sum::<usize>();
    let model = &model_group.main;
    let max_output = model
        .request_settings
        .max_tokens
        .unwrap_or(model_group.inference.default_max_tokens) as usize;
    let budget = model_group
        .context_window
        .saturating_sub(max_output)
        .saturating_sub(estimate_tokens(system_prompt))
        * model_group.inference.history_truncation_pct
        / 100;
    if tokens > budget {
        return Err(AppError::Validation(format!(
            "Chat context exceeds the configured history limit ({tokens} estimated tokens, {budget} available); the current request has been preserved"
        )));
    }
    Ok(())
}

fn summary_tokens_for(content: &str) -> usize {
    estimate_message_tokens(&summary_message(content))
}

fn ensure_fits(tokens: usize, available: usize) -> Result<(), AppError> {
    if tokens > available {
        return Err(AppError::Validation(format!(
            "Chat context exceeds the available context limit ({tokens} estimated tokens, {available} available); the current request has been preserved"
        )));
    }
    Ok(())
}

fn is_substantive_request(msg: &Message) -> bool {
    matches!(
        msg.role,
        MessageRole::User
            | MessageRole::Contact
            | MessageRole::LiveCall
            | MessageRole::TaskCompletion
    ) && !matches!(msg.command, Some(MessageCommand::Command { .. }))
        && (!msg.content.trim().is_empty() || !msg.attachments.is_empty())
}

fn timestamp_group_start(messages: &[Message], mut index: usize) -> usize {
    while index > 0 && messages[index - 1].created_at == messages[index].created_at {
        index -= 1;
    }
    index
}
