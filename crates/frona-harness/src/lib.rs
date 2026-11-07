//! Database-independent models, tools, adapters, and shared budget accounting.
//!
//! Await configured runs for text or typed output, or use the same configuration
//! for manual execution. Both drivers share admission, conversation and dispatch.

pub mod config;
pub mod handle;
pub mod inference;
pub mod retry_config;
pub use handle::{Handle, InvalidHandle};
mod automatic;
mod budget;
mod conversation;
mod dispatch;
mod execution;
mod hook;
mod model;
mod refinement;
mod session;
mod submission;
mod tool;

pub use automatic::Incomplete;
pub use refinement::Decision;

pub use budget::{
    Budget, BudgetCompletion, BudgetLimit, BudgetLimits, BudgetOperation, BudgetOutcome,
    BudgetPermit, BudgetRefusal, BudgetSnapshot, DefaultBudget, UncertainUsagePolicy,
};
pub use conversation::{
    ActionKind, ActionRecord, ActionResolution, ActionStatus, Conversation, ConversationError,
    ConversationStatus, PendingAction, PendingActionView, ResponseActions, ResponseRecord,
};
pub use dispatch::ToolDispatchError;
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
    Harness, ManualError, ManualSession, ModelCall, Run, Structured, Text, ToolSelection,
};

/// Initialize the TLS provider for built-in clients. Safe to call more than once.
pub fn initialize_tls() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
