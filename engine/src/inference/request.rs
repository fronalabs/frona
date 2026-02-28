use rig::completion::Message as RigMessage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::models::Agent;
use crate::chat::models::Chat;
use crate::core::models::user::User;
use crate::tool::registry::AgentToolRegistry;

use super::config::ModelGroup;
use super::registry::ModelProviderRegistry;
use super::tool_loop::{InferenceEvent, ToolCallResult};

pub struct InferenceRequest<'a> {
    pub registry: &'a ModelProviderRegistry,
    pub model_group: &'a ModelGroup,
    pub system_prompt: &'a str,
    pub history: Vec<RigMessage>,
    pub tool_registry: &'a AgentToolRegistry,
    pub user: &'a User,
    pub agent: &'a Agent,
    pub chat: &'a Chat,
    pub event_tx: mpsc::Sender<InferenceEvent>,
    pub cancel_token: CancellationToken,
}

#[derive(Debug)]
pub enum InferenceResponse {
    Completed {
        text: String,
        attachments: Vec<crate::api::files::Attachment>,
    },
    Cancelled(String),
    ExternalToolPending {
        accumulated_text: String,
        tool_calls_json: serde_json::Value,
        tool_results: Vec<ToolCallResult>,
        external_tool: Box<ToolCallResult>,
        system_prompt: Option<String>,
    },
}
