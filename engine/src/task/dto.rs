use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::models::TaskStatus;

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub chat_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
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
    pub chat_id: Option<String>,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<super::models::Task> for TaskResponse {
    fn from(task: super::models::Task) -> Self {
        Self {
            id: task.id,
            chat_id: task.chat_id,
            title: task.title,
            description: task.description,
            status: task.status,
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }
}
