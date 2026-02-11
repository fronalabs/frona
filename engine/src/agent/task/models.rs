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

impl TaskKind {
    pub fn source_chat_id(&self) -> Option<&str> {
        match self {
            TaskKind::Direct => None,
            TaskKind::Delegation { source_chat_id, .. } => Some(source_chat_id),
            TaskKind::Cron { source_chat_id, .. } => source_chat_id.as_deref(),
        }
    }
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

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub agent_id: String,
    pub space_id: Option<String>,
    pub chat_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub source_agent_id: Option<String>,
    pub source_chat_id: Option<String>,
    pub deliver_directly: Option<bool>,
    pub run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTaskRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<TaskStatus>,
}

#[derive(Debug, Serialize)]
pub struct TaskResponse {
    pub id: String,
    pub agent_id: String,
    pub space_id: Option<String>,
    pub chat_id: Option<String>,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub run_at: Option<DateTime<Utc>>,
    pub result_summary: Option<String>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Task> for TaskResponse {
    fn from(task: Task) -> Self {
        Self {
            id: task.id,
            agent_id: task.agent_id,
            space_id: task.space_id,
            chat_id: task.chat_id,
            title: task.title,
            description: task.description,
            status: task.status,
            kind: task.kind,
            run_at: task.run_at,
            result_summary: task.result_summary,
            error_message: task.error_message,
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_chat_id_direct() {
        assert_eq!(TaskKind::Direct.source_chat_id(), None);
    }

    #[test]
    fn source_chat_id_delegation() {
        let kind = TaskKind::Delegation {
            source_agent_id: "a1".to_string(),
            source_chat_id: "c1".to_string(),
            deliver_directly: false,
        };
        assert_eq!(kind.source_chat_id(), Some("c1"));
    }

    #[test]
    fn source_chat_id_cron_with_value() {
        let kind = TaskKind::Cron {
            cron_expression: "0 9 * * *".to_string(),
            next_run_at: None,
            source_agent_id: None,
            source_chat_id: Some("c2".to_string()),
        };
        assert_eq!(kind.source_chat_id(), Some("c2"));
    }

    #[test]
    fn source_chat_id_cron_without_value() {
        let kind = TaskKind::Cron {
            cron_expression: "0 9 * * *".to_string(),
            next_run_at: None,
            source_agent_id: None,
            source_chat_id: None,
        };
        assert_eq!(kind.source_chat_id(), None);
    }
}
