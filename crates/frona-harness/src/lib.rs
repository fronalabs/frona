//! Database-independent models, tools, adapters, and shared budget accounting.
//!
//! Model operations enforce shared admission. Execution drivers own conversation,
//! tool dispatch, and recovery orchestration.

pub mod config;
pub mod handle;
pub mod inference;
pub mod retry_config;
pub use handle::{Handle, InvalidHandle};
mod budget;
mod execution;
mod model;
mod tool;

pub use budget::{
    Budget, BudgetCompletion, BudgetLimit, BudgetLimits, BudgetOperation, BudgetOutcome,
    BudgetPermit, BudgetRefusal, BudgetSnapshot, DefaultBudget, UncertainUsagePolicy,
};
pub use execution::{ExecutionControl, ExecutionStop};
pub use model::{
    Capabilities, Capability, Model, ModelAdapter, ModelBuilder, ModelError, ModelOutput,
    ModelRequest, TypedOutput, UsageReport,
};
pub use tool::{
    IntoToolOutput, Tool, ToolContent, ToolContext, ToolError, ToolFailure, ToolOutput,
    ToolRegistrationError, ToolRegistry, ToolResult, TypedTool, tool, tool_with_context,
};

/// Initialize the TLS provider for built-in clients. Safe to call more than once.
pub fn initialize_tls() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
