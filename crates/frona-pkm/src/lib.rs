#![doc = include_str!("../README.md")]

pub mod evidence;
pub mod ingest;
pub mod model;
mod path;
mod recall;
mod source;
mod stats;

pub use evidence::ToolEvidenceProjection;
pub use recall::RecallProjection;
pub use source::{TemporalSource, TranscriptEvidenceKind, TranscriptEvidenceSource};
pub use stats::ResearchCoverageStats;

/// A reasoning phase with caller-owned input and output. An error must not advance
/// the caller's durable progress. Successful output is still uncommitted data.
pub trait Phase: Sync {
    type Input: Send;
    type Output: Send;
    fn run(&self, input: Self::Input) -> impl Future<Output = Result<Self::Output, Error>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Internal(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("cancelled: {0}")]
    Cancelled(String),
}

mod graph;
pub mod ontology;
pub mod view;
pub use graph::GraphSnapshot;
pub mod page;
pub mod writeback;

pub mod candidates;
pub mod classify;
pub mod projection;
pub mod reconcile;

pub mod playbook;

mod tools;

pub mod resolve;

pub mod assemble;

impl From<frona_text::prompt::PromptError> for Error {
    fn from(error: frona_text::prompt::PromptError) -> Self {
        Self::Internal(error.to_string())
    }
}

mod prompt;

mod redaction;
#[cfg(test)]
mod test_support;
