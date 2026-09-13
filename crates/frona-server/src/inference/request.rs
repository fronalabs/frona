use rig_core::completion::Message as RigMessage;
use tokio_util::sync::CancellationToken;

use crate::agent::models::Agent;
use crate::agent::task::models::Task;
use crate::auth::User;
use crate::chat::broadcast::EventSender;
use crate::chat::models::Chat;
use crate::core::error::AppError;
use crate::tool::registry::AgentToolRegistry;

use crate::inference::ModelGroup;
use crate::inference::tool_call::TaskEvent;
use crate::inference::usage::UsageService;

use crate::chat::message::models::Reasoning;

pub struct InferenceContext {
    pub user: User,
    pub agent: Agent,
    /// The originating chat, or `None` for a **detached** inference (background /
    /// sync passes that have no chat - e.g. the PKM sync investigator). Chat-scoped
    /// tools reach it via [`active_chat`], which refuses cleanly when it's `None`.
    pub chat: Option<Chat>,
    pub task: Option<Task>,
    pub event_tx: EventSender,
    /// Resolved filesystem paths for files shared in this chat (from message attachments).
    pub file_paths: Vec<String>,
    pub shutdown_token: CancellationToken,
    /// User-initiated cancellation token - tools should check/use this to abort early.
    pub cancel_token: CancellationToken,
}

impl InferenceContext {
    /// The normal chat-bound context. Callers keep passing a `Chat`; it's wrapped
    /// `Some` so every existing chat-path caller is unaffected.
    pub fn new(
        user: User,
        agent: Agent,
        chat: Chat,
        event_tx: EventSender,
        shutdown_token: CancellationToken,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            user,
            agent,
            chat: Some(chat),
            task: None,
            event_tx,
            file_paths: Vec::new(),
            shutdown_token,
            cancel_token,
        }
    }

    /// A detached (chatless) context for background/sync inference. There's no chat
    /// to stream to, so it wires its own [`EventSender::noop`]. Chat-scoped tools
    /// invoked here refuse via [`active_chat`].
    pub fn new_detached(
        user: User,
        agent: Agent,
        shutdown_token: CancellationToken,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            user,
            agent,
            chat: None,
            task: None,
            event_tx: EventSender::noop(),
            file_paths: Vec::new(),
            shutdown_token,
            cancel_token,
        }
    }

    pub fn with_task(mut self, task: Task) -> Self {
        self.task = Some(task);
        self
    }
}

/// Borrow the active chat, or refuse - the choke point for chat-scoped tools on a
/// detached context. A **free function** (not a method) deliberately: the erroring
/// `AppError` policy doesn't belong on the context data type.
pub fn active_chat(ctx: &InferenceContext) -> Result<&Chat, AppError> {
    ctx.chat
        .as_ref()
        .ok_or_else(|| AppError::Tool("this tool requires an active chat".into()))
}

pub struct InferenceRequest {
    pub model_group: ModelGroup,
    pub system_prompt: String,
    pub history: Vec<RigMessage>,
    pub tool_registry: AgentToolRegistry,
    pub ctx: InferenceContext,
    pub cancel_token: CancellationToken,
    pub chat_service: crate::chat::service::ChatService,
    pub message_id: String,
    pub usage_service: UsageService,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum InferenceResponse {
    Completed {
        text: String,
        attachments: Vec<crate::storage::Attachment>,
        lifecycle_event: Option<TaskEvent>,
        reasoning: Option<Reasoning>,
    },
    Cancelled(String),
    ExternalToolPending {
        turn_text: String,
        tool_calls: Vec<crate::inference::tool_call::ToolCallResponse>,
        system_prompt: Option<String>,
    },
    /// The harness already wrote the response for this turn - `finalize`
    /// should leave the placeholder agent message alone. Used by the
    /// `Command` dispatch path (`/clear`, `/compact`, `/title`, etc.) which
    /// either completes the placeholder inline or wipes it as a side effect.
    Handled,
}
