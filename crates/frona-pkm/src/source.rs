#[derive(Debug, Clone)]
pub struct TemporalSource {
    pub handle: String,
    pub text: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Present only for task lifecycle sources. Outcome episodes resolve here.
    pub task_event_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Present when the task had a deferred run or recurring fire time.
    pub task_target_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone)]
pub struct TranscriptEvidenceSource {
    pub handle: String,
    pub text: String,
    pub kind: TranscriptEvidenceKind,
}

#[derive(Debug, Clone)]
pub enum TranscriptEvidenceKind {
    UserMessage {
        message_id: String,
        chat_id: String,
    },
    AgentMessage {
        message_id: String,
        agent_id: String,
        chat_id: String,
    },
    TaskLifecycle {
        message_id: String,
        chat_id: String,
        task_id: String,
    },
    ExternalNote {
        note: String,
    },
}
