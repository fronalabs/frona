//! Reliable, synchronous transition commits. Encoding and file I/O are backend
//! concerns. A commit never outlives its caller or acknowledges queued work.

use std::sync::Arc;

use rig_core::completion::{message::UserContent, request::ToolDefinition};
use uuid::Uuid;

use super::{ActionPosition, ActionResolution, ModelOutput, ToolOutput};

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryMode {
    Text,
    Structured(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionCompletion {
    Schema,
    Application,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryCompletion {
    Text { response: usize },
    Protocol,
}

/// Owned, unencoded transition data. Compaction/checkpoint events are added by
/// their implementation tasks; no serialization is required by memory history.
#[derive(Debug, Clone)]
pub enum HistoryEvent {
    Opened {
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
        acknowledgement: ToolOutput,
        kind: SubmissionCompletion,
    },
    Finished {
        kind: HistoryCompletion,
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
    Finish,
    Stop,
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
            Self::Finished { .. } => HistoryOperation::Finish,
            Self::Stopped { .. } => HistoryOperation::Stop,
        }
    }
}

#[derive(Debug, Clone)]
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

    #[cfg(test)]
    pub(crate) fn attach(&mut self, writer: Box<dyn HistoryWriter>) {
        assert!(
            self.memory.records.is_empty() && self.failure.is_none(),
            "attach history before execution"
        );
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
