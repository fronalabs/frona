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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::task::models::TaskStatus;

    #[test]
    fn agent_role_serializes_lowercase() {
        let json = serde_json::to_string(&MessageRole::Agent).unwrap();
        assert_eq!(json, r#""agent""#);
    }

    #[test]
    fn agent_role_deserializes_from_lowercase() {
        let role: MessageRole = serde_json::from_str(r#""agent""#).unwrap();
        assert_eq!(role, MessageRole::Agent);
    }

    #[test]
    fn task_completion_role_serializes_lowercase() {
        let json = serde_json::to_string(&MessageRole::TaskCompletion).unwrap();
        assert_eq!(json, r#""taskcompletion""#);
    }

    #[test]
    fn task_completion_role_deserializes_from_lowercase() {
        let role: MessageRole = serde_json::from_str(r#""taskcompletion""#).unwrap();
        assert_eq!(role, MessageRole::TaskCompletion);
    }

    #[test]
    fn task_completion_tool_round_trips() {
        let tool = MessageTool::TaskCompletion {
            task_id: "t-123".to_string(),
            chat_id: Some("chat-456".to_string()),
            status: TaskStatus::Completed,
        };

        let json = serde_json::to_string(&tool).unwrap();
        let parsed: MessageTool = serde_json::from_str(&json).unwrap();

        match parsed {
            MessageTool::TaskCompletion {
                task_id,
                chat_id,
                status,
            } => {
                assert_eq!(task_id, "t-123");
                assert_eq!(chat_id, Some("chat-456".to_string()));
                assert_eq!(status, TaskStatus::Completed);
            }
            _ => panic!("Expected TaskCompletion variant"),
        }
    }

    #[test]
    fn task_completion_tool_with_null_chat_id() {
        let tool = MessageTool::TaskCompletion {
            task_id: "t-789".to_string(),
            chat_id: None,
            status: TaskStatus::Failed,
        };

        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains(r#""type":"TaskCompletion""#));

        let parsed: MessageTool = serde_json::from_str(&json).unwrap();
        match parsed {
            MessageTool::TaskCompletion { chat_id, status, .. } => {
                assert_eq!(chat_id, None);
                assert_eq!(status, TaskStatus::Failed);
            }
            _ => panic!("Expected TaskCompletion variant"),
        }
    }

    #[test]
    fn task_completion_tool_with_task_status() {
        for status in [TaskStatus::Completed, TaskStatus::Failed] {
            let tool = MessageTool::TaskCompletion {
                task_id: "t-1".to_string(),
                chat_id: None,
                status: status.clone(),
            };
            let json = serde_json::to_string(&tool).unwrap();
            let parsed: MessageTool = serde_json::from_str(&json).unwrap();
            match parsed {
                MessageTool::TaskCompletion { status: s, .. } => assert_eq!(s, status),
                _ => panic!("Expected TaskCompletion"),
            }
        }
    }

    #[test]
    fn attachment_round_trips() {
        let attachment = Attachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size_bytes: 1024,
            path: "user://uid-123/report.pdf".to_string(),
        };
        let json = serde_json::to_string(&attachment).unwrap();
        let parsed: Attachment = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.filename, "report.pdf");
        assert_eq!(parsed.path, "user://uid-123/report.pdf");
        assert_eq!(parsed.size_bytes, 1024);
    }

    #[test]
    fn message_with_empty_attachments_round_trips() {
        let msg = Message {
            id: "m1".to_string(),
            chat_id: "c1".to_string(),
            role: MessageRole::User,
            content: "hello".to_string(),
            agent_id: None,
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments: vec![],
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(parsed.attachments.is_empty());
    }

    #[test]
    fn message_with_attachments_round_trips() {
        let msg = Message {
            id: "m1".to_string(),
            chat_id: "c1".to_string(),
            role: MessageRole::User,
            content: "see attached".to_string(),
            agent_id: None,
            tool_calls: None,
            tool_call_id: None,
            tool: None,
            attachments: vec![
                Attachment {
                    filename: "photo.png".to_string(),
                    content_type: "image/png".to_string(),
                    size_bytes: 2048,
                    path: "user://uid/photo.png".to_string(),
                },
            ],
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename, "photo.png");
    }

    #[test]
    fn message_without_attachments_field_defaults_to_empty() {
        let json = r#"{
            "id": "m1",
            "chat_id": "c1",
            "role": "user",
            "content": "old message",
            "agent_id": null,
            "tool_calls": null,
            "tool_call_id": null,
            "tool": null,
            "created_at": "2024-01-01T00:00:00Z"
        }"#;
        let parsed: Message = serde_json::from_str(json).unwrap();
        assert!(parsed.attachments.is_empty());
    }
}
