use chrono::{DateTime, Utc};
use crate::Entity;
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[serde(rename_all = "lowercase")]
#[surreal(crate = "surrealdb::types", lowercase)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, SurrealValue)]
#[serde(tag = "type")]
#[surreal(crate = "surrealdb::types", tag = "type")]
pub enum TaskKind {
    #[default]
    Direct,
    Delegation {
        source_agent_id: String,
        source_chat_id: String,
        #[serde(default)]
        deliver_directly: bool,
    },
    Cron {
        cron_expression: String,
        next_run_at: Option<DateTime<Utc>>,
        source_agent_id: Option<String>,
        source_chat_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "task")]
pub struct Task {
    pub id: String,
    pub user_id: String,
    pub agent_id: String,
    pub space_id: Option<String>,
    pub chat_id: Option<String>,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub kind: TaskKind,
    #[serde(default)]
    pub run_at: Option<DateTime<Utc>>,
    pub result_summary: Option<String>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_kind_serialization() {
        let direct = TaskKind::Direct;
        let json = serde_json::to_string(&direct).unwrap();
        let deserialized: TaskKind = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, TaskKind::Direct));

        let delegation = TaskKind::Delegation {
            source_agent_id: "agent-1".to_string(),
            source_chat_id: "chat-1".to_string(),
            deliver_directly: false,
        };
        let json = serde_json::to_string(&delegation).unwrap();
        let deserialized: TaskKind = serde_json::from_str(&json).unwrap();
        match deserialized {
            TaskKind::Delegation { source_agent_id, source_chat_id, deliver_directly } => {
                assert_eq!(source_agent_id, "agent-1");
                assert_eq!(source_chat_id, "chat-1");
                assert!(!deliver_directly);
            }
            _ => panic!("Expected Delegation variant"),
        }

        let cron = TaskKind::Cron {
            cron_expression: "0 9 * * *".to_string(),
            next_run_at: Some(Utc::now()),
            source_agent_id: Some("agent-1".to_string()),
            source_chat_id: Some("chat-1".to_string()),
        };
        let json = serde_json::to_string(&cron).unwrap();
        let deserialized: TaskKind = serde_json::from_str(&json).unwrap();
        match deserialized {
            TaskKind::Cron { cron_expression, source_agent_id, .. } => {
                assert_eq!(cron_expression, "0 9 * * *");
                assert_eq!(source_agent_id, Some("agent-1".to_string()));
            }
            _ => panic!("Expected Cron variant"),
        }

    }

    #[test]
    fn test_task_status_serialization() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, status);
        }
    }

    #[test]
    fn delegation_with_deliver_directly_serialization() {
        let kind = TaskKind::Delegation {
            source_agent_id: "a1".to_string(),
            source_chat_id: "c1".to_string(),
            deliver_directly: true,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let deserialized: TaskKind = serde_json::from_str(&json).unwrap();
        match deserialized {
            TaskKind::Delegation { deliver_directly, .. } => assert!(deliver_directly),
            _ => panic!("Expected Delegation"),
        }
    }

    #[test]
    fn delegation_without_deliver_directly_defaults_false() {
        let json = r#"{"type":"Delegation","source_agent_id":"a1","source_chat_id":"c1"}"#;
        let deserialized: TaskKind = serde_json::from_str(json).unwrap();
        match deserialized {
            TaskKind::Delegation { deliver_directly, .. } => assert!(!deliver_directly),
            _ => panic!("Expected Delegation"),
        }
    }
}
