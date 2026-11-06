//! Database-independent model and tool construction and adapter contracts.
//!
//! These are single-invocation building blocks, not agent loops. Retry, budget
//! admission, history, and recovery belong to the shared execution layer.

pub mod config;
pub mod handle;
pub mod inference;
pub mod retry_config;
pub use handle::{Handle, InvalidHandle};
mod model;
mod tool;

pub use model::{
    Capabilities, Capability, Model, ModelAdapter, ModelBuilder, ModelError, ModelOutput,
    ModelRequest, TypedOutput, UsageReport,
};
pub use tool::{
    ExecutionStop, IntoToolOutput, Tool, ToolContent, ToolContext, ToolError, ToolFailure,
    ToolOutput, ToolRegistrationError, ToolRegistry, ToolResult, TypedTool, tool,
    tool_with_context,
};

/// Initialize the TLS provider for built-in clients. Safe to call more than once.
pub fn initialize_tls() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
