//! Background consolidation - turns a completed transcript into KB structure.
//!
//! A pass follows one explicit stage sequence:
//!
//! `Ingest → Classify → Resolve → Reconcile → Assemble → PlaybookResolve →`
//! `PlaybookAuthor → PageAuthor → Cleanup → Done`.
//!
//! Ingest runs per chat. The later stages run once per user. The sweep commits each
//! extracted batch with its watermark, so a mining or commit failure leaves the
//! watermark where it is.
//!
//! # What survives a failure
//!
//! Two mechanisms, covering different things. Neither subsumes the other.
//!
//! **Per item - the dirty set** (`updated_at > rendered_at`). Stages are deliberately
//! tolerant of one item failing: an entity that will not classify must not stop the other
//! ninety-nine. Only [`PageAuthor`] stamps `rendered_at`, and it stamps per entity when the
//! exact article bytes commit, so anything that failed earlier is still owed and the next pass
//! picks it up. Every write that changes what an entity *renders* - retiring a memory,
//! quarantining it, unioning it onto a survivor - bumps the entities it affects, so an entity
//! made stale by another entity's reconcile carries a signal too.
//!
//! **Per stage - the [`KnowledgeConsolidationRecord`]**. A stage that fails outright stops the
//! pass and parks the record on that stage; the sweep backs off and resumes *there*.
//! The record also banks what each stage finished, so a resumed pass does not re-pay for
//! it - which matters for stages that spend model calls per item. Past its retry budget
//! the pass is abandoned outright: an entity that fails
//! deterministically would otherwise wedge this user's memory forever, and nothing is
//! lost by starting over, because whatever it did not finish is still dirty.
//!
//! Core model: memories are canonical; entities are reconstructed from the memories
//! linked to them each pass. Prompt templates are read fresh on every use, so editing
//! a file under `resources/prompts/pkm/` takes effect on the next pass.
//!

pub(crate) mod checkpoint;
mod cleanup;
mod concepts;
mod context;
mod driver;
mod inference;
mod playbook;
mod scope;
mod stats;

use super::page::PageAuthor;

pub use crate::memory::pkm::model::PendingEntityContribution;
pub use checkpoint::{
    ConsolidationFailure, ConsolidationStageState, ConsolidationWorkState, IngestState,
    KnowledgeConsolidationRecord, PlaybookResolveState,
};

pub(crate) use context::ConsolidationContext;
pub(crate) use inference::ConsolidationInference;

pub use driver::Consolidator;
pub use scope::ConsolidationScope;
pub use stats::{
    AssembleOutcome, ClassifyOutcome, CleanupOutcome, ConsolidationStats, PageAuthorOutcome,
    PlaybookAuthorOutcome, ReconcileOutcome, ResearchCoverageStats, ResolveOutcome,
};
