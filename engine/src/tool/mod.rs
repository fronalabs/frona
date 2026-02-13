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
use crate::core::error::AppError;
use crate::inference::tool_loop::ToolLoopEvent;
use crate::core::models::user::User;

use crate::agent::prompt::PromptLoader;
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

fn parse_frontmatter(raw: &str) -> Option<(Value, String)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = &trimmed[3..];
    let end = after_first.find("---")?;
    let yaml_str = &after_first[..end];
    let body = after_first[end + 3..].trim().to_string();
    let yaml: Value = serde_yaml::from_str(yaml_str).ok()?;
    Some((yaml, body))
}

fn build_parameters_json(yaml: &Value) -> Value {
    let params = yaml.get("parameters").cloned().unwrap_or(Value::Null);
    let required = yaml.get("required").cloned().unwrap_or(Value::Null);

    let properties: Value = if let Value::Object(map) = &params {
        let mut props = serde_json::Map::new();
        for (key, schema) in map {
            props.insert(key.clone(), serde_json::to_value(schema).unwrap_or(Value::Null));
        }
        Value::Object(props)
    } else {
        Value::Object(serde_json::Map::new())
    };

    let mut result = serde_json::json!({
        "type": "object",
        "properties": properties,
    });

    if let Value::Array(arr) = &required {
        let req: Vec<Value> = arr.iter().map(|v| {
            if let Value::String(s) = v {
                Value::String(s.clone())
            } else {
                v.clone()
            }
        }).collect();
        result["required"] = Value::Array(req);
    }

    result
}

pub fn load_tool_definition(prompts: &PromptLoader, path: &str) -> Option<ToolDefinition> {
    let raw = prompts.read(path)?;
    let (yaml, body) = parse_frontmatter(&raw)?;
    let name = yaml.get("name")?.as_str()?.to_string();
    let parameters = build_parameters_json(&yaml);
    Some(ToolDefinition {
        name,
        description: body,
        parameters,
    })
}
