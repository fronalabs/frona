use crate::memory::pkm::vault::VaultScope;

use super::RecallProjection;

/// Who/what this consolidation pass is for.
#[derive(Clone)]
pub struct ConsolidationScope {
    pub user_id: String,
    /// Owner display name (from the `User` record) - seeds the self-entity and the
    /// extractor's owner→path binding. May be blank at first contact.
    pub user_name: String,
    pub agent_id: String,
    /// The originating chat, or `None` for a detached pass (sync/External ingest -
    /// runs as the `system` agent with no chat). This is domain provenance;
    /// inner SDK dialogues do not create chat/message rows.
    pub chat_id: Option<String>,
    /// Where this user's files live - handle, Memory directory, absolute root. Resolved
    /// once per pass rather than re-derived at each use site. See [`VaultScope`].
    pub vault: VaultScope,
    /// Hidden from model prompts. Used only after extraction to resolve a verbatim
    /// episodic anchor against the message that contained it.
    pub temporal_sources: Vec<TemporalSource>,
    /// Prompt-local transcript handles and the durable records they identify. Hidden from
    /// the model except for each handle; Ingest resolves submitted citations through it.
    pub evidence_sources: Vec<TranscriptEvidenceSource>,
    /// Successful foreground knowledge retrievals associated with Agent messages in
    /// this extraction window. Prompt-local and never persisted with the checkpoint.
    pub recall: RecallProjection,
    /// IANA timezone used for deterministic calendar arithmetic.
    pub timezone: String,
}

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
