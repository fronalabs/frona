use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use super::{ConsolidationWorkState, IngestState, PlaybookResolveState};

/// Which stage the pass is in, and what that stage has finished.
///
/// One variant per stage, matching `Consolidator::run`'s pipeline order. Classify,
/// Resolve, Reconcile, and Assemble carry the same durable work state between them.
///
/// A stage has a payload only when its completion is not visible in the live tables.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub enum ConsolidationStageState {
    Ingest(IngestState),
    Classify(ConsolidationWorkState),
    Resolve(ConsolidationWorkState),
    Reconcile(ConsolidationWorkState),
    Assemble(ConsolidationWorkState),
    PlaybookResolve(PlaybookResolveState),
    /// Stateless dirty-page fan-out, parallel to Page Author.
    PlaybookAuthor,
    /// Nothing to remember: author stamps `rendered_at` with the exact article bytes in
    /// one database commit, and the worklist is `updated_at > rendered_at` re-read at
    /// entry - so a resumed author already skips what it finished, with no bookkeeping.
    PageAuthor,
    /// Pure function of live state and idempotent, so it has nothing to remember.
    Cleanup,
    /// Terminal. The row is kept as the pass log - `stats` is its payload.
    Done,
    /// Terminal semantic failure. Memories and transcript watermarks survive for Repair.
    Failed,
}

impl ConsolidationStageState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ingest(_) => "ingest",
            Self::Classify(_) => "classify",
            Self::Resolve(_) => "resolve",
            Self::Reconcile(_) => "reconcile",
            Self::Assemble(_) => "assemble",
            Self::PlaybookResolve(_) => "playbook_resolve",
            Self::PlaybookAuthor => "playbook_author",
            Self::PageAuthor => "page_author",
            Self::Cleanup => "cleanup",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }

    pub(crate) fn revision(&self) -> Option<u64> {
        match self {
            Self::Classify(state)
            | Self::Resolve(state)
            | Self::Reconcile(state)
            | Self::Assemble(state) => Some(state.revision),
            Self::PlaybookResolve(state) => Some(state.revision),
            _ => None,
        }
    }

    /// The state a stage hands to the next one. The driver writes this between every
    /// stage, so a resume never lands worse than a stage boundary.
    pub fn next(&self) -> Self {
        match self {
            Self::Ingest(_) => Self::Classify(ConsolidationWorkState::default()),
            Self::Classify(state) => Self::Resolve(state.clone()),
            Self::Resolve(state) => Self::Reconcile(state.clone()),
            Self::Reconcile(state) => Self::Assemble(state.clone()),
            Self::Assemble(_) => Self::PlaybookResolve(PlaybookResolveState::default()),
            Self::PlaybookResolve(_) => Self::PlaybookAuthor,
            Self::PlaybookAuthor => Self::PageAuthor,
            Self::PageAuthor => Self::Cleanup,
            Self::Cleanup | Self::Done => Self::Done,
            Self::Failed => Self::Failed,
        }
    }
}
