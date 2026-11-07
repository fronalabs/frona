//! Reliable, synchronous transition commits. Encoding and file I/O are backend
//! concerns. A commit never outlives its caller or acknowledges queued work.

use std::sync::Arc;

use rig_core::completion::{message::UserContent, request::ToolDefinition};
use uuid::Uuid;

use super::{ActionPosition, ActionResolution, ModelOutput, ToolOutput};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum HistoryMode {
    Text,
    Structured(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SubmissionCompletion {
    Schema,
    Application,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HistoryCompletion {
    Text { response: usize },
    Protocol,
}

/// Owned transition data shared by memory history and the versioned codec.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum HistoryEvent {
    Opened {
        #[serde(deserialize_with = "crate::history::required")]
        initial: Option<Vec<UserContent>>,
        instructions: String,
        mode: HistoryMode,
    },
    User {
        message: usize,
        content: Vec<UserContent>,
    },
    Response {
        response: usize,
        message: usize,
        #[serde(deserialize_with = "crate::history::required")]
        origin: Option<String>,
        offered_tools: Vec<ToolDefinition>,
        output: ModelOutput,
    },
    ToolStarted {
        action: ActionPosition,
        ordinal: u64,
    },
    ActionResolved {
        action: ActionPosition,
        resolution: ActionResolution,
    },
    SubmissionCompleted {
        action: ActionPosition,
        #[serde(with = "tool_output")]
        acknowledgement: ToolOutput,
        kind: SubmissionCompletion,
    },
    Compacted {
        view: super::CompactedView,
        summarizer: String,
        output: ModelOutput,
    },
    Finished {
        kind: HistoryCompletion,
    },
    Checkpoint {
        checkpoint: HistoryCheckpoint,
    },
    Stopped {
        category: super::FailureKind,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryOperation {
    Open,
    User,
    Response,
    ToolStart,
    ActionResolution,
    SubmissionCompletion,
    Compact,
    Finish,
    Stop,
    Checkpoint,
}

impl HistoryEvent {
    pub fn operation(&self) -> HistoryOperation {
        match self {
            Self::Opened { .. } => HistoryOperation::Open,
            Self::User { .. } => HistoryOperation::User,
            Self::Response { .. } => HistoryOperation::Response,
            Self::ToolStarted { .. } => HistoryOperation::ToolStart,
            Self::ActionResolved { .. } => HistoryOperation::ActionResolution,
            Self::SubmissionCompleted { .. } => HistoryOperation::SubmissionCompletion,
            Self::Compacted { .. } => HistoryOperation::Compact,
            Self::Finished { .. } => HistoryOperation::Finish,
            Self::Stopped { .. } => HistoryOperation::Stop,
            Self::Checkpoint { .. } => HistoryOperation::Checkpoint,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryRecord {
    pub version: u32,
    pub conversation: Uuid,
    pub seq: u64,
    pub event: HistoryEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryWritePhase {
    Serialize,
    Append,
    Flush,
    Synchronize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceOutcome {
    NotWritten,
    Unknown,
}

/// SDK-controlled model/tool dispatch associated with the failed transition.
/// This does not describe prior inspector/application side effects or rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectState {
    Prevented,
    InProgress,
    Returned,
    /// This transition has no associated SDK model/tool dispatch. Application
    /// code, including an inspector, may already have produced its own effects.
    NoEffect,
}

/// The last acknowledged position is a lower bound, never proof that a failed
/// write left no bytes. The unsaved candidate is available on the conversation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{operation:?} record {attempted_seq} failed during {phase:?}: {cause}")]
pub struct HistoryFailure {
    pub acknowledged_seq: Option<u64>,
    pub attempted_seq: u64,
    pub operation: HistoryOperation,
    pub phase: HistoryWritePhase,
    pub persistence: PersistenceOutcome,
    pub effect: EffectState,
    pub cause: String,
}

#[derive(Debug)]
pub(crate) struct HistoryWriteError {
    pub phase: HistoryWritePhase,
    pub cause: String,
}

/// Required writes are synchronous and complete before dependent state/effects.
/// Future file backends must include append/flush/sync here; this is deliberately
/// not a nonblocking-I/O promise. Keeping the boundary internal avoids a storage
/// plugin API before the concrete file and persistent-runtime adapters exist.
pub(crate) trait HistoryWriter: Send + Sync + std::fmt::Debug {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError>;
}

#[derive(Debug, Default)]
pub(crate) struct MemoryHistory {
    records: Vec<HistoryRecord>,
}

#[derive(Debug, Default)]
pub(crate) struct History {
    memory: MemoryHistory,
    writer: Option<Box<dyn HistoryWriter>>,
    failure: Option<Arc<HistoryFailure>>,
    candidate: Option<HistoryRecord>,
    detached_failures: Vec<Arc<HistoryFailure>>,
}

impl History {
    pub(crate) fn from_records(records: Vec<HistoryRecord>) -> Self {
        Self {
            memory: MemoryHistory { records },
            ..Self::default()
        }
    }

    pub fn records(&self) -> &[HistoryRecord] {
        &self.memory.records
    }
    pub fn failure(&self) -> Option<&Arc<HistoryFailure>> {
        self.failure.as_ref()
    }
    pub fn candidate(&self) -> Option<&HistoryRecord> {
        self.candidate.as_ref()
    }
    pub fn detached_failures(&self) -> &[Arc<HistoryFailure>] {
        &self.detached_failures
    }

    #[cfg(any(test, feature = "jsonl"))]
    pub(crate) fn attach(&mut self, writer: Box<dyn HistoryWriter>) {
        assert!(self.can_attach_new(), "attach history before execution");
        self.writer = Some(writer);
    }

    #[cfg(any(test, feature = "jsonl"))]
    pub(crate) fn can_attach_new(&self) -> bool {
        self.memory.records.is_empty()
            && self.writer.is_none()
            && self.failure.is_none()
            && self.candidate.is_none()
            && self.detached_failures.is_empty()
    }

    #[cfg(feature = "jsonl")]
    pub(crate) fn attach_restored(&mut self, writer: Box<dyn HistoryWriter>) {
        assert!(self.writer.is_none() && self.failure.is_none() && self.candidate.is_none());
        self.writer = Some(writer);
    }

    pub fn commit(
        &mut self,
        conversation: Uuid,
        event: HistoryEvent,
        effect: EffectState,
    ) -> Result<(), Arc<HistoryFailure>> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let record = HistoryRecord {
            version: 1,
            conversation,
            seq: self.memory.records.len() as u64,
            event,
        };
        if let Some(writer) = &mut self.writer
            && let Err(error) = writer.commit(&record)
        {
            let failure = Arc::new(HistoryFailure {
                acknowledged_seq: self.memory.records.last().map(|r| r.seq),
                attempted_seq: record.seq,
                operation: record.event.operation(),
                phase: error.phase,
                persistence: if error.phase == HistoryWritePhase::Serialize {
                    PersistenceOutcome::NotWritten
                } else {
                    PersistenceOutcome::Unknown
                },
                effect,
                cause: error.cause,
            });
            self.candidate = Some(record);
            self.failure = Some(failure.clone());
            // The uncertain writer can never be used again by this state.
            self.writer = None;
            return Err(failure);
        }
        self.memory.records.push(record);
        Ok(())
    }

    pub fn detach(&mut self) -> bool {
        let Some(failure) = self.failure.take() else {
            return false;
        };
        self.detached_failures.push(failure);
        self.writer = None;
        true
    }

    pub fn take_candidate(&mut self) -> Option<HistoryRecord> {
        self.candidate.take()
    }
}

/// Opaque application checkpoint associated with the immediately preceding prefix.
/// Application schema compatibility and typed decoding belong to the caller.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryCheckpoint {
    pub schema: String,
    pub version: u64,
    #[serde(deserialize_with = "crate::history::required")]
    pub payload: serde_json::Value,
    pub through_seq: u64,
}

// ToolOutput deliberately does not implement Serialize: its IntoToolOutput impl
// must remain distinct from the blanket serializable-result convenience impl.
pub(super) mod tool_output {
    use super::ToolOutput;
    use serde::{Deserialize, Deserializer, Serializer, ser::SerializeStruct};
    #[derive(Deserialize)]
    struct Wire {
        content: Vec<super::super::ToolContent>,
        #[serde(deserialize_with = "crate::history::required")]
        metadata: serde_json::Value,
    }
    pub fn serialize<S: Serializer>(output: &ToolOutput, serializer: S) -> Result<S::Ok, S::Error> {
        let mut wire = serializer.serialize_struct("ToolOutput", 2)?;
        wire.serialize_field("content", &output.content)?;
        wire.serialize_field("metadata", &output.metadata)?;
        wire.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ToolOutput, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        Ok(ToolOutput {
            content: wire.content,
            metadata: wire.metadata,
        })
    }
}

// Require the field to exist even when its value may explicitly be null. Serde's
// default Option/Value missing-field behavior would hide truncated payloads.
pub(super) fn required<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer)
}
