#![doc = include_str!("../README.md")]

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
