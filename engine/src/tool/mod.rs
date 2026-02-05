pub mod browser;
pub mod cli;
pub mod delegate;
pub mod heartbeat;
pub mod notify_human;
pub mod produce_file;
pub mod read_file;
pub mod registry;
pub mod remember;
pub mod schedule;
pub mod skill;
pub mod time;
pub mod update_entity;
pub mod update_identity;
pub mod web_fetch;
pub mod web_search;
pub mod workspace;

use std::sync::OnceLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::models::Agent;
use crate::chat::models::Chat;
use crate::error::AppError;
use crate::llm::tool_loop::ToolLoopEvent;
use crate::models::user::User;

use self::cli::CliToolConfig;

static CONFIGURABLE_TOOLS: OnceLock<Vec<String>> = OnceLock::new();

pub fn init_configurable_tools(cli_tools: &[CliToolConfig]) {
    let mut names: Vec<String> = cli_tools.iter().map(|t| t.name.clone()).collect();
    names.push("browser".to_string());
    names.push("web_fetch".to_string());
    names.push("web_search".to_string());
    names.push("delegate".to_string());
    names.push("schedule".to_string());
    names.push("heartbeat".to_string());
    let _ = CONFIGURABLE_TOOLS.set(names);
}

pub fn configurable_tools() -> &'static [String] {
    CONFIGURABLE_TOOLS.get().map(|v| v.as_slice()).unwrap_or(&[])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    Internal,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub struct ImageData {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

pub struct ToolOutput {
    text: String,
    images: Vec<ImageData>,
    attachments: Vec<crate::api::files::Attachment>,
    tool_data: Option<crate::chat::message::models::MessageTool>,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            images: Vec::new(),
            attachments: Vec::new(),
            tool_data: None,
        }
    }

    pub fn mixed(text: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self {
            text: text.into(),
            images,
            attachments: Vec::new(),
            tool_data: None,
        }
    }

    pub fn with_attachment(mut self, a: crate::api::files::Attachment) -> Self {
        self.attachments.push(a);
        self
    }

    pub fn with_tool_data(mut self, td: crate::chat::message::models::MessageTool) -> Self {
        self.tool_data = Some(td);
        self
    }

    pub fn text_content(&self) -> &str {
        &self.text
    }

    pub fn images(&self) -> &[ImageData] {
        &self.images
    }

    pub fn attachments(&self) -> &[crate::api::files::Attachment] {
        &self.attachments
    }

    pub fn tool_data(&self) -> Option<&crate::chat::message::models::MessageTool> {
        self.tool_data.as_ref()
    }
}

pub struct ToolContext {
    pub user: User,
    pub agent: Agent,
    pub chat: Chat,
    pub event_tx: mpsc::Sender<ToolLoopEvent>,
}

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn tool_type(&self, _tool_name: &str) -> ToolType {
        ToolType::Internal
    }
    async fn execute(&self, tool_name: &str, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AppError>;
    async fn cleanup(&self) -> Result<(), AppError> {
        Ok(())
    }
}
