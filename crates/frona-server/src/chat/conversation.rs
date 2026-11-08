//! Persistent history is a frozen request prefix, not a fabricated SDK event log.
//!
//! The adapter preserves native builder output and stored-row provenance. New
//! model responses/actions belong to the shared SDK Conversation; prefix-local
//! positions are not its action IDs or evidence of live feedback delivery.
//!
//! Runtime integration must append `Conversation::request_messages()` from the
//! new live turn to this prefix, then record only new responses/actions through
//! that Conversation. A fresh `Conversation::default()` avoids repeating the
//! input already present in the persisted prefix. Prefix positions must never
//! be passed as live original positions when recording report delivery.
//!
//! Controlled preparation shares the live budget, cancellation and report with
//! the main turn, including actual summary generation and provider retries.

use frona_harness::{
    HistoryView, ModelRequest,
    inference::{ModelGroup, context::InputTokenEstimate},
};
use rig_core::completion::request::ToolDefinition;

use crate::{
    agent::context::AgentContextScope,
    core::error::AppError,
    inference::conversation::{ConversationBuilder, ConversationContext, ModelConversationHistory},
};

use super::{
    compactor::{ChatCompactor, CompactionContext, validate_initial_context},
    message::models::Message,
    service::ChatService,
};

/// Canonical rows and their exact native projection, including saved context.
/// No model/tool, live source capture, or message update occurs while loading.
pub struct PersistentConversation {
    chat_id: String,
    stored_messages: Vec<Message>,
    model_history: ModelConversationHistory,
    input_token_estimate: Option<InputTokenEstimate>,
    compacted: bool,
}

/// Live dependencies are supplied only at the explicit compaction boundary.
pub struct PersistentRequest<'a> {
    pub agent_context: AgentContextScope<'a>,
    pub system_prompt: &'a str,
    pub model_group: &'a ModelGroup,
    pub tools: &'a [ToolDefinition],
}

impl PersistentConversation {
    /// Load only through the requested persisted row, excluding later arrivals.
    /// Builders keep ownership of attachments, cross-agent projection, and frozen
    /// processed text. Summary context is loaded by signature, never recollected.
    pub async fn load(
        service: &ChatService,
        chat_id: &str,
        through_message: Option<&str>,
        builder: &dyn ConversationBuilder,
        context: &ConversationContext,
    ) -> Result<Self, AppError> {
        let loaded = service.compactor().load_conversation(chat_id).await?;
        let mut stored_messages = loaded.messages;
        if let Some(id) = through_message {
            let index = stored_messages
                .iter()
                .position(|message| message.id == id)
                .ok_or_else(|| {
                    AppError::Conflict("Requested row is outside the active conversation".into())
                })?;
            stored_messages.truncate(index + 1);
        }
        let tool_calls = service.get_tool_calls(chat_id).await?;
        let mut model_history = builder
            .build(
                &stored_messages,
                &tool_calls,
                context,
                loaded.summary.as_deref(),
            )
            .await;
        model_history.validate_source(&stored_messages, loaded.summary.as_deref())?;
        model_history.set_summary_context(loaded.agent_context.as_deref());
        HistoryView::persisted(&model_history.history).map_err(invalid_history)?;
        Ok(Self {
            chat_id: chat_id.into(),
            stored_messages,
            model_history,
            input_token_estimate: None,
            compacted: false,
        })
    }

    /// Selected persisted rows in the same order as `model_history().row_ids`.
    /// Compacted rows remain untouched in the canonical repository.
    pub fn stored_messages(&self) -> &[Message] {
        &self.stored_messages
    }
    pub fn model_history(&self) -> &ModelConversationHistory {
        &self.model_history
    }
    pub fn into_model_history(self) -> ModelConversationHistory {
        self.model_history
    }
    pub fn input_token_estimate(&self) -> Option<InputTokenEstimate> {
        self.input_token_estimate
    }
    pub fn was_compacted(&self) -> bool {
        self.compacted
    }

    /// Prepare a shared SDK request after optional asynchronous compaction. The
    /// compactor commits its canonical summary/context before exposing the new
    /// view; a failure leaves this adapter's previously selected history intact.
    /// This performs no main-model invocation or tool dispatch.
    pub async fn prepare(
        &mut self,
        compactor: &ChatCompactor,
        request: PersistentRequest<'_>,
    ) -> Result<ModelRequest, AppError> {
        let budget = frona_harness::DefaultBudget::default();
        self.prepare_controlled(
            compactor,
            request,
            frona_harness::ExecutionControl::new(&budget),
        )
        .await
    }

    pub async fn prepare_controlled(
        &mut self,
        compactor: &ChatCompactor,
        request: PersistentRequest<'_>,
        execution: frona_harness::ExecutionControl<'_>,
    ) -> Result<ModelRequest, AppError> {
        let outcome = compactor
            .compact_chat_controlled(
                &self.chat_id,
                CompactionContext {
                    agent_context: request.agent_context,
                    system_prompt: request.system_prompt,
                    model_group: request.model_group,
                    model_history: &self.model_history,
                    tools: request.tools,
                },
                execution,
            )
            .await?;
        validate_initial_context(
            &outcome.model_history.history,
            request.system_prompt,
            request.model_group,
            request.tools,
            outcome.input_token_estimate,
        )?;
        let messages = HistoryView::persisted(&outcome.model_history.history)
            .map_err(invalid_history)?
            .messages();
        self.stored_messages
            .retain(|message| outcome.model_history.row_ids.contains(&message.id));
        self.model_history = outcome.model_history;
        self.input_token_estimate = outcome.input_token_estimate;
        self.compacted = outcome.compacted;
        Ok(ModelRequest {
            system: request.system_prompt.into(),
            messages,
            tools: request.tools.to_vec(),
            schema: None,
            // Keep per-model request settings, including fallback output limits.
            max_tokens: None,
            temperature: None,
        })
    }
}

fn invalid_history(error: frona_harness::CompactionError) -> AppError {
    AppError::Validation(format!("Invalid persisted conversation: {error}"))
}
