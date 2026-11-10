//! Database-independent model/tool runs and application-controlled loops.
//!
//! Prepare a [`Model`](crate::Model) once, then await [`Harness::run`](crate::Harness::run) for text or use
//! [`Run::structured`](crate::Run::structured) for typed output. Both automatic and manual execution
//! share admission, conversation state, and dispatch.
//!
//! ```no_run
//! use frona_harness::{Harness, Model};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Deserialize, JsonSchema)]
//! struct Summary { title: String }
//!
//! # async fn example(model: Model<'static>) -> Result<(), Box<dyn std::error::Error>> {
//! let harness = Harness::new(model);
//! let summary = harness.run("Give the notes a title").structured::<Summary>().await?;
//! # Ok(())
//! # }
//! ```
//!
//! - Register application tools with [`tool`](fn@crate::tool) or [`workspace_read_tools`](crate::workspace_read_tools); none
//!   are installed by default. [`Run::before_tool`](crate::Run::before_tool) can execute, reject, or stop.
//! - Use [`Run::inspect`](crate::Run::inspect) for asynchronous application feedback. Only explicit
//!   [`Decision::accept`](crate::Decision::accept) completes refinement; [`Incomplete`](crate::Incomplete) retention remains
//!   unfinished and requires application revalidation before any domain commit.
//! - Use [`Run::manual`](crate::Run::manual) to decide individual actions and select each model call.
//!   [`ModelAdapter`](crate::ModelAdapter) wraps application-owned clients; tools and inspectors may
//!   borrow local state. Spawning requires an owned, `Send`, static future.
//! - Reuse [`DefaultBudget`](crate::DefaultBudget) or implement [`Budget`](crate::Budget) to share allowance across
//!   runs and nested calls. Admission happens before every actual model/tool call.
//!   Already-admitted calls may finish after budget exhaustion.
//! - Compaction is enabled by default; [`Run::disable_compaction`](crate::Run::disable_compaction) suppresses
//!   summaries while retaining context overflow checks. [`Run::subscribe`](crate::Run::subscribe) adds
//!   optional bounded progress without changing the final result.
//! - With `jsonl`, file history can be loaded without execution, then explicitly
//!   continued through [`Harness::resume`](crate::Harness::resume). Restore application dependencies and
//!   budget policy first; checkpoints do not restore budget usage or acceptance.
//!   The caller coordinates writers and makes interrupted tools safe to repeat.

extern crate self as frona_harness;

#[doc(hidden)]
pub mod __private {
    pub use super::tools::annotated::{AnnotatedFuture, AnnotatedTool};
    pub use schemars;
    pub use serde;
}

pub mod config;
pub mod handle;
pub mod inference;
pub mod retry_config;
pub mod tools;
pub use handle::{Handle, InvalidHandle};
mod automatic;
mod budget;
mod compaction;
mod conversation;
mod execution;
mod history;
mod history_codec;
#[cfg(feature = "jsonl")]
mod jsonl;
pub use history_codec::{
    DecodedHistory, HistoryDecodeError, HistoryDecodeErrorKind, decode_jsonl, encode_jsonl,
};
#[cfg(feature = "jsonl")]
pub use jsonl::{Jsonl, JsonlError, JsonlOperation, JsonlWriter};
mod model;
mod progress;
mod refinement;
mod report;
mod session;
mod submission;

pub use automatic::Incomplete;
pub use progress::{Progress, ProgressReceiver, RefinementStage};
pub use refinement::Decision;
pub use report::{
    ActionOutcome, ActionPosition, ActionReport, AttemptId, AttemptReport, CallOutcome,
    ExecutionReport, FailureKind, FeedbackReport, ReportHandle, RequestId, RequestInput,
    RequestReport, RunOutcome,
};

pub use budget::{
    Budget, BudgetCompletion, BudgetLimit, BudgetLimits, BudgetOperation, BudgetOutcome,
    BudgetPermit, BudgetRefusal, BudgetSnapshot, DefaultBudget, UncertainUsagePolicy,
};
pub use conversation::{
    ActionKind, ActionRecord, ActionResolution, ActionStatus, CheckpointLoadError, CheckpointRef,
    Conversation, ConversationError, ConversationInput, ConversationStatus, HistoryReplay,
    LoadedCheckpoint, PendingAction, PendingActionView, ResponseActions, ResponseRecord,
    replay_history,
};
pub use execution::{ExecutionControl, ExecutionStop};
pub use model::{
    Capabilities, Capability, Model, ModelAdapter, ModelBuilder, ModelError, ModelOutput,
    ModelRequest, TypedOutput, UsageReport,
};
pub use submission::{Submission, SubmissionError, submission_tool};
#[doc(inline)]
pub use tools::tool;
pub use tools::{
    IntoTool, IntoToolOutput, RecoveryError, Tool, ToolCallHook, ToolContent, ToolContext,
    ToolDecision, ToolDispatchError, ToolError, ToolFailure, ToolFuture, ToolOutput,
    ToolRegistrationError, ToolRegistry, ToolResult, TypedTool, tool_with_context,
    workspace_read_tools,
};

pub use session::{
    Harness, ManualError, ManualSession, ModelCall, ResumeError, Run, Structured, Text,
    ToolSelection,
};

pub use history::{
    EffectState, HistoryCheckpoint, HistoryCompletion, HistoryEvent, HistoryFailure, HistoryMode,
    HistoryOperation, HistoryRecord, HistoryWritePhase, PersistenceOutcome, SubmissionCompletion,
};

pub use compaction::{
    CompactedView, CompactionConfig, CompactionError, CompactionLimits, CompactionPlan,
    HistoryView, HistoryViewEntry,
};

/// Initialize the TLS provider for built-in clients. Safe to call more than once.
pub fn initialize_tls() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
