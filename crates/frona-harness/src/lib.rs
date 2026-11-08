//! Database-independent models, tools, adapters, and shared budget accounting.
//!
//! Await configured runs for text or typed output, or use the same configuration
//! for manual execution. Both drivers share admission, conversation and dispatch.

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
    Conversation, ConversationError, ConversationStatus, HistoryReplay, LoadedCheckpoint,
    PendingAction, PendingActionView, ResponseActions, ResponseRecord, replay_history,
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
