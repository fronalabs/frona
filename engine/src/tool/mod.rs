pub mod browser;
pub mod cli;
pub mod notify_human;
pub mod registry;
pub mod remember;
pub mod skill;
pub mod update_entity;
pub mod web_fetch;
pub mod web_search;
pub mod workspace;

use std::sync::OnceLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppError;

use self::cli::CliToolConfig;

static CONFIGURABLE_TOOLS: OnceLock<Vec<String>> = OnceLock::new();

pub fn init_configurable_tools(cli_tools: &[CliToolConfig]) {
    let mut names: Vec<String> = cli_tools.iter().map(|t| t.name.clone()).collect();
    names.push("browser".to_string());
    names.push("web_fetch".to_string());
    names.push("web_search".to_string());
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

pub enum ToolOutput {
    Text(String),
    Mixed { text: String, images: Vec<ImageData> },
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput::Text(s.into())
    }

    pub fn text_content(&self) -> &str {
        match self {
            ToolOutput::Text(s) | ToolOutput::Mixed { text: s, .. } => s,
        }
    }

    pub fn images(&self) -> &[ImageData] {
        match self {
            ToolOutput::Text(_) => &[],
            ToolOutput::Mixed { images, .. } => images,
        }
    }
}

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn tool_type(&self, _tool_name: &str) -> ToolType {
        ToolType::Internal
    }
    async fn execute(&self, tool_name: &str, arguments: Value) -> Result<ToolOutput, AppError>;
    async fn cleanup(&self) -> Result<(), AppError> {
        Ok(())
    }
}
