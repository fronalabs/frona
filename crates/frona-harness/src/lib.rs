//! Database-independent models, tools, adapters, and shared budget accounting.
//!
//! Await configured runs for text or typed output, or use the same configuration
//! for manual execution. Both drivers share admission, conversation and dispatch.

extern crate self as frona_harness;

mod annotated_tool;
pub use annotated_tool::IntoTool;
/// Define a portable tool from an async free function. Its function name and doc
/// comments become tool metadata, and owned parameters become named JSON fields.
/// A `ToolContext` parameter is injected automatically, at any position.
///
/// ```
/// use frona_harness::{tool, ToolContext, ToolError, ToolRegistry};
/// /// Add two integers.
/// #[tool]
/// async fn add(left: i64, right: i64, execution: ToolContext<'_>) -> Result<i64, ToolError> {
///     let _ = execution.call_id;
///     Ok(left + right)
/// }
/// let mut tools = ToolRegistry::default();
/// tools.register(add)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// Functions remain directly callable and registration performs no invocation.
/// Use `#[tool(name = "lookup", description = "Find a record")]` to override
/// metadata. Renamed Cargo dependencies are detected automatically; an explicit
/// `crate = "path"` option supports re-exported SDK paths.
///
/// Supported signatures are safe async free functions with a `Result` output,
/// owned model arguments, no generic parameters, and at most 16 parameters total.
/// `ToolContext<'_>` or `&ToolContext<'_>` may be qualified by a module path;
/// renamed imports/type aliases for that type are not detected. Explicit static
/// context lifetimes and mutable context references are rejected. Other types
/// are model arguments, never automatically injected application state. Use the
/// closure helpers for captured dependencies or implement `Tool` for lending state.
/// Each call returns a metadata-bearing future that boxes the original async body;
/// awaiting it preserves the original `Result` type. Metadata is not recovered
/// after erasing that return type to a general boxed future.
#[doc(inline)]
pub use frona_derive::tool;

#[doc(hidden)]
pub mod __private {
    pub use super::annotated_tool::{AnnotatedFuture, AnnotatedTool};
    pub use schemars;
    pub use serde;
}

pub mod config;
pub mod file_tools;
pub mod handle;
pub mod inference;
pub mod retry_config;
pub use handle::{Handle, InvalidHandle};
mod automatic;
mod budget;
mod compaction;
mod conversation;
mod dispatch;
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
mod hook;
mod model;
mod progress;
mod refinement;
mod report;
mod session;
mod submission;
mod tool;

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
pub use dispatch::{RecoveryError, ToolDispatchError};
pub use execution::{ExecutionControl, ExecutionStop};
pub use hook::{ToolCallHook, ToolDecision};
pub use model::{
    Capabilities, Capability, Model, ModelAdapter, ModelBuilder, ModelError, ModelOutput,
    ModelRequest, TypedOutput, UsageReport,
};
pub use submission::{Submission, SubmissionError, submission_tool};
pub use tool::{
    IntoToolOutput, Tool, ToolContent, ToolContext, ToolError, ToolFailure, ToolFuture, ToolOutput,
    ToolRegistrationError, ToolRegistry, ToolResult, TypedTool, tool, tool_with_context,
};

pub use session::{
    Harness, ManualError, ManualSession, ModelCall, ResumeError, Run, Structured, Text,
    ToolSelection,
};

mod workspace_tools;
pub use workspace_tools::workspace_read_tools;

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
