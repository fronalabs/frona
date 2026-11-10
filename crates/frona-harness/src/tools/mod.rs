//! Tool definitions, registration, dispatch and opt-in workspace operations.
//!
//! Registration never executes a tool. Dispatch shares the run's conversation,
//! admission and recovery state; filesystem operations leave authorization to
//! the caller. Common tool types are also available at the crate root.

pub(crate) mod annotated;
mod context;
mod dispatch;
pub mod files;
mod hook;
mod registry;
mod tool;
mod typed;
mod workspace;

pub use annotated::IntoTool;
pub use context::ToolContext;
pub use dispatch::{RecoveryError, ToolDispatchError};
pub use hook::{ToolCallHook, ToolDecision};
pub use registry::{ToolRegistrationError, ToolRegistry};
pub use tool::{IntoToolOutput, Tool, ToolContent, ToolError, ToolFailure, ToolOutput, ToolResult};
pub use typed::{ToolFuture, TypedTool, tool, tool_with_context};
pub use workspace::workspace_read_tools;

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

pub(crate) use hook::CallbackHook;
