use chrono::{DateTime, Utc};
use crate::Entity;
use crate::api::files::Attachment;
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "lowercase")]
#[surreal(crate = "surrealdb::types", lowercase)]
pub enum MessageRole {
    User,
    Agent,
    ToolResult,
    TaskCompletion,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "lowercase")]
#[surreal(crate = "surrealdb::types", lowercase)]
pub enum ToolStatus {
    Pending,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[serde(tag = "type", content = "data")]
#[surreal(crate = "surrealdb::types", tag = "type", content = "data")]
pub enum MessageTool {
    HumanInTheLoop {
        reason: String,
        debugger_url: String,
        status: ToolStatus,
        response: Option<String>,
    },
    Question {
        question: String,
        options: Vec<String>,
        status: ToolStatus,
        response: Option<String>,
    },
    TaskCompletion {
        task_id: String,
        chat_id: Option<String>,
        status: crate::agent::task::models::TaskStatus,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "message")]
pub struct Message {
    pub id: String,
    pub chat_id: String,
    pub role: MessageRole,
    pub content: String,
    pub agent_id: Option<String>,
    pub tool_calls: Option<serde_json::Value>,
    pub tool_call_id: Option<String>,
    pub tool: Option<MessageTool>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    pub created_at: DateTime<Utc>,
}

impl Message {
    pub fn builder(chat_id: &str, role: MessageRole, content: String) -> MessageBuilder {
        MessageBuilder {
            chat_id: chat_id.to_string(),
            role,
            content,
            agent_id: None,
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments: vec![],
        }
    }
}

pub struct MessageBuilder {
    chat_id: String,
    role: MessageRole,
    content: String,
    agent_id: Option<String>,
    tool_calls: Option<serde_json::Value>,
    tool_call_id: Option<String>,
    tool: Option<MessageTool>,
    attachments: Vec<Attachment>,
}

impl MessageBuilder {
    pub fn agent_id(mut self, id: String) -> Self {
        self.agent_id = Some(id);
        self
    }

    pub fn tool_calls(mut self, tc: serde_json::Value) -> Self {
        self.tool_calls = Some(tc);
        self
    }

    pub fn tool_call_id(mut self, id: String) -> Self {
        self.tool_call_id = Some(id);
        self
    }

    pub fn tool(mut self, t: MessageTool) -> Self {
        self.tool = Some(t);
        self
    }

    pub fn attachments(mut self, a: Vec<Attachment>) -> Self {
        self.attachments = a;
        self
    }

    pub fn build(self) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: self.chat_id,
            role: self.role,
            content: self.content,
            agent_id: self.agent_id,
            tool_calls: self.tool_calls,
            tool_call_id: self.tool_call_id,
            tool: self.tool,
            attachments: self.attachments,
            created_at: chrono::Utc::now(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Deserialize)]
pub struct ResolveToolRequest {
    pub response: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageResponse {
    pub id: String,
    pub chat_id: String,
    pub role: MessageRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<MessageTool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    pub created_at: DateTime<Utc>,
}

impl From<Message> for MessageResponse {
    fn from(msg: Message) -> Self {
        Self {
            id: msg.id,
            chat_id: msg.chat_id,
            role: msg.role,
            content: msg.content,
            agent_id: msg.agent_id,
            tool_calls: msg.tool_calls,
            tool_call_id: msg.tool_call_id,
            tool: msg.tool,
            attachments: msg.attachments,
            created_at: msg.created_at,
        }
    }
}
