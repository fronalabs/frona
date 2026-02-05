use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::models::{TaskKind, TaskStatus};

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

impl From<super::models::Task> for TaskResponse {
    fn from(task: super::models::Task) -> Self {
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
