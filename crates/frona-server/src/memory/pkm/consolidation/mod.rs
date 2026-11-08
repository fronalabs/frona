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
//! ninety-nine. Only [`page`] stamps `rendered_at`, and it stamps per entity when the
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
//! [`page`]: page::PageAuthor

mod assemble;
mod classify;
mod cleanup;
mod context;
mod driver;
mod inference;
mod ingest;
mod page;
mod playbook;
mod progress;
mod projection;
mod prompt;
mod recall;
mod reconcile;
mod resolve;
mod scope;
mod stages;
mod stats;
pub(crate) mod transcript;
pub use recall::RecallProjection;
mod evidence;
pub use evidence::ToolEvidenceProjection;
mod candidates;
pub(crate) mod checkpoint;
pub(crate) mod tools;
pub(crate) mod view;

pub(super) use page::PageAuthor;

pub use crate::memory::pkm::model::{PendingEntityContribution, PendingPlaybookCandidate};
pub use checkpoint::{
    ConsolidationFailure, ConsolidationStageState, IngestState, KnowledgeConsolidationRecord,
};
pub use playbook::PlaybookResolveState;
pub use progress::ConsolidationWorkState;

use progress::{ConsolidationProgress, ReconcilePromotion};
use projection::*;
use stages::{ResolveWorkState, bad_term_feedback};

pub(crate) use context::ConsolidationContext;
pub(crate) use inference::ConsolidationInference;
pub(crate) use prompt::{PromptIds, PromptSpec, prompt_evidence};

pub(crate) use checkpoint::prepare_ingest_batch;
pub use driver::Consolidator;
pub use ingest::Ingest;
pub use scope::{
    ConsolidationScope, TemporalSource, TranscriptEvidenceKind, TranscriptEvidenceSource,
};
pub use stats::{
    AssembleOutcome, ClassifyOutcome, CleanupOutcome, ConsolidationStats, PageAuthorOutcome,
    PlaybookAuthorOutcome, ReconcileOutcome, ResearchCoverageStats, ResolveOutcome,
};

/// Fold a string to a loose comparison key: trimmed, internal whitespace collapsed,
/// lowercased.
///
/// The one rule two stages need for the same reason - a model that re-types a value it
/// was shown will re-space and re-capitalise it. Reconcile matches a quoted `was`/`now`
/// against the entry it claims to come from; the Classify stage matches a mention's name
/// against a candidate entity's name or alias. Both had their own copy, identical to the
/// character and named differently, so neither read as the shared rule it is.
///
/// Deliberately not a stemmer or a slug: this is for comparing two spellings of the same
/// text, not for matching paraphrases (see `grounding`) or minting a path (`slugify`).
pub(super) fn comparison_key(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
