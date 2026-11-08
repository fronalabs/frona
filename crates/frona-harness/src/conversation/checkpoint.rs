//! Explicit application state associated with one acknowledged history prefix.

use serde::{Serialize, de::DeserializeOwned};

use super::{Conversation, ConversationError};
use crate::{EffectState, HistoryCheckpoint, HistoryEvent, HistoryOperation};

/// Borrowed evidence for the latest checkpoint. Raw payload access remains
/// available when typed loading fails, so the caller can reconcile deliberately.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointRef<'a> {
    pub seq: u64,
    pub checkpoint: &'a HistoryCheckpoint,
    pub stale: bool,
}

/// Decoded application data, not an accepted refinement result or continuation.
#[derive(Debug, PartialEq, Eq)]
pub struct LoadedCheckpoint<T> {
    pub seq: u64,
    pub through_seq: u64,
    pub payload: T,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointLoadError {
    #[error("checkpoint {seq} uses application schema {schema:?} version {version}")]
    Incompatible {
        seq: u64,
        schema: String,
        version: u64,
    },
    #[error("checkpoint {seq} is stale: execution continued after prefix {through_seq}")]
    Stale { seq: u64, through_seq: u64 },
    #[error("cannot decode checkpoint {seq}: {source}")]
    Decode {
        seq: u64,
        #[source]
        source: serde_json::Error,
    },
}

impl CheckpointRef<'_> {
    /// Check application identity, then freshness, then decode. No older
    /// checkpoint is selected on failure; the original evidence remains readable.
    pub fn load<T: DeserializeOwned>(
        self,
        schema: &str,
        version: u64,
    ) -> Result<LoadedCheckpoint<T>, CheckpointLoadError> {
        if self.checkpoint.schema != schema || self.checkpoint.version != version {
            return Err(CheckpointLoadError::Incompatible {
                seq: self.seq,
                schema: self.checkpoint.schema.clone(),
                version: self.checkpoint.version,
            });
        }
        if self.stale {
            return Err(CheckpointLoadError::Stale {
                seq: self.seq,
                through_seq: self.checkpoint.through_seq,
            });
        }
        let payload = T::deserialize(&self.checkpoint.payload).map_err(|source| {
            CheckpointLoadError::Decode {
                seq: self.seq,
                source,
            }
        })?;
        Ok(LoadedCheckpoint {
            seq: self.seq,
            through_seq: self.checkpoint.through_seq,
            payload,
        })
    }
}

impl Conversation {
    /// Save only explicitly supplied application data. The returned sequence
    /// identifies this checkpoint; its through_seq is the preceding record.
    /// Checkpoints may follow completion and never accept or reactivate work.
    /// This is one reliable history write, not a transaction with app effects.
    pub fn save_checkpoint<T: Serialize + ?Sized>(
        &mut self,
        schema: impl Into<String>,
        version: u64,
        payload: &T,
    ) -> Result<u64, ConversationError> {
        self.ensure_history()?;
        let payload = serde_json::to_value(payload).map_err(|error| {
            let failure = self
                .history
                .serialization_failed(HistoryOperation::Checkpoint, error.to_string());
            self.mark_stopped();
            ConversationError::Storage(failure)
        })?;
        let through_seq = self.history().last().expect("Opened was ensured").seq;
        self.commit(
            HistoryEvent::Checkpoint {
                checkpoint: HistoryCheckpoint {
                    schema: schema.into(),
                    version,
                    payload,
                    through_seq,
                },
            },
            EffectState::NoEffect,
        )?;
        Ok(self.history().last().expect("checkpoint committed").seq)
    }

    /// Latest acknowledged checkpoint only. Failed candidates are never exposed
    /// as saved state, and later checkpoints supersede all earlier schemas.
    pub fn checkpoint(&self) -> Option<CheckpointRef<'_>> {
        let mut stale = false;
        for record in self.history().iter().rev() {
            if let HistoryEvent::Checkpoint { checkpoint } = &record.event {
                return Some(CheckpointRef {
                    seq: record.seq,
                    checkpoint,
                    stale,
                });
            }
            stale = true;
        }
        None
    }

    /// Load current compatible app state, or distinguish absent, incompatible,
    /// stale, and unreadable state. This does not restore application control flow.
    pub fn load_checkpoint<T: DeserializeOwned>(
        &self,
        schema: &str,
        version: u64,
    ) -> Result<Option<LoadedCheckpoint<T>>, CheckpointLoadError> {
        self.checkpoint()
            .map(|checkpoint| checkpoint.load(schema, version))
            .transpose()
    }
}
