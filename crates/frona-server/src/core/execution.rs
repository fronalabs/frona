use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::chat::broadcast::BroadcastService;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionKind {
    Inference,
    Task,
    Memory,
    App,
    Scheduled,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionStatus {
    Queued,
    Running,
    Waiting,
    Cancelling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionSourceKind {
    Chat,
    Task,
    Schedule,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionSource {
    #[serde(rename = "type")]
    pub kind: ExecutionSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    pub id: String,
    pub title: String,
    pub kind: ExecutionKind,
    pub status: ExecutionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ExecutionSource>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_chat_ids: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub can_cancel: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivitySnapshot {
    pub executions: Vec<Execution>,
}

#[derive(Debug, Clone)]
pub struct NewExecution {
    pub title: String,
    pub kind: ExecutionKind,
    pub action: Option<String>,
    pub source: Option<ExecutionSource>,
    pub related_chat_ids: Vec<String>,
    pub can_cancel: bool,
}

#[derive(Debug, Clone)]
struct OwnedExecution {
    user_id: String,
    execution: Execution,
}

#[derive(Clone)]
pub struct ExecutionRegistry {
    entries: Arc<Mutex<HashMap<String, OwnedExecution>>>,
    broadcast: BroadcastService,
}

impl ExecutionRegistry {
    pub fn new(broadcast: BroadcastService) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            broadcast,
        }
    }

    pub fn start(&self, user_id: &str, new: NewExecution) -> ExecutionGuard {
        let id = crate::core::repository::new_id();
        let execution = Execution {
            id: id.clone(),
            title: new.title,
            kind: new.kind,
            status: ExecutionStatus::Running,
            action: new.action,
            source: new.source,
            related_chat_ids: new.related_chat_ids,
            started_at: Utc::now(),
            can_cancel: new.can_cancel,
        };
        self.entries.lock().unwrap().insert(
            id.clone(),
            OwnedExecution {
                user_id: user_id.to_string(),
                execution,
            },
        );
        self.broadcast.broadcast_activity_changed(user_id);
        ExecutionGuard {
            id: Some(id),
            user_id: user_id.to_string(),
            registry: self.clone(),
        }
    }

    pub fn snapshot(&self, user_id: &str) -> ActivitySnapshot {
        let mut executions = self
            .entries
            .lock()
            .unwrap()
            .values()
            .filter(|entry| entry.user_id == user_id)
            .map(|entry| entry.execution.clone())
            .collect::<Vec<_>>();
        executions.sort_by_key(|execution| execution.started_at);
        ActivitySnapshot { executions }
    }

    fn update(
        &self,
        user_id: &str,
        id: &str,
        status: Option<ExecutionStatus>,
        action: Option<Option<String>>,
    ) {
        let changed = {
            let mut entries = self.entries.lock().unwrap();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            if entry.user_id != user_id {
                return;
            }
            if let Some(status) = status {
                entry.execution.status = status;
            }
            if let Some(action) = action {
                entry.execution.action = action;
            }
            true
        };
        if changed {
            self.broadcast.broadcast_activity_changed(user_id);
        }
    }

    fn finish(&self, user_id: &str, id: &str) {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            if entries
                .get(id)
                .is_some_and(|entry| entry.user_id == user_id)
            {
                entries.remove(id);
                true
            } else {
                false
            }
        };
        if removed {
            self.broadcast.broadcast_activity_changed(user_id);
        }
    }
}

pub struct ExecutionGuard {
    id: Option<String>,
    user_id: String,
    registry: ExecutionRegistry,
}

impl ExecutionGuard {
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or("")
    }

    pub fn set_status(&self, status: ExecutionStatus) {
        if let Some(id) = self.id.as_deref() {
            self.registry.update(&self.user_id, id, Some(status), None);
        }
    }

    pub fn set_action(&self, action: Option<String>) {
        if let Some(id) = self.id.as_deref() {
            self.registry.update(&self.user_id, id, None, Some(action));
        }
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.registry.finish(&self.user_id, &id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution(title: &str) -> NewExecution {
        NewExecution {
            title: title.to_string(),
            kind: ExecutionKind::Inference,
            action: Some("Generating response".to_string()),
            source: Some(ExecutionSource {
                kind: ExecutionSourceKind::Chat,
                id: Some("chat-1".to_string()),
            }),
            related_chat_ids: vec!["chat-1".to_string()],
            can_cancel: true,
        }
    }

    #[tokio::test]
    async fn snapshots_are_user_scoped_and_guards_remove_executions() {
        let registry = ExecutionRegistry::new(BroadcastService::new());
        let first = registry.start("user-1", execution("First"));
        let _second = registry.start("user-2", execution("Second"));

        assert_eq!(registry.snapshot("user-1").executions.len(), 1);
        assert_eq!(registry.snapshot("user-1").executions[0].title, "First");

        drop(first);
        assert!(registry.snapshot("user-1").executions.is_empty());
        assert_eq!(registry.snapshot("user-2").executions.len(), 1);
    }

    #[tokio::test]
    async fn guard_updates_the_authoritative_snapshot() {
        let registry = ExecutionRegistry::new(BroadcastService::new());
        let guard = registry.start("user-1", execution("First"));

        guard.set_status(ExecutionStatus::Waiting);
        guard.set_action(Some("Waiting for approval".to_string()));

        let snapshot = registry.snapshot("user-1");
        assert_eq!(snapshot.executions[0].status, ExecutionStatus::Waiting);
        assert_eq!(
            snapshot.executions[0].action.as_deref(),
            Some("Waiting for approval")
        );
    }
}
