//! Explicit, read-only file discovery and inspection for one workspace.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use super::files;
use super::{ToolError, ToolRegistry, tool};

/// Register `read` and `glob` for an existing workspace directory. Relative
/// paths start at this root; absolute paths must resolve inside it. Symlinks
/// are resolved before checking the boundary, and Glob does not follow links.
///
/// This grants read access to the supplied directory only. It initializes no
/// identities, database, process sandbox, shell, or other tool services. The
/// check does not protect against concurrent filesystem path replacement.
/// Runtime tools retain their separate Cedar policy and explicit sibling grants.
///
/// ```no_run
/// # use frona_harness::{Harness, Model, workspace_read_tools};
/// # async fn example(model: Model<'static>) -> Result<(), Box<dyn std::error::Error>> {
/// let answer = Harness::new(model)
///     .run("Find and summarize the project notes")
///     .tools(workspace_read_tools("./notes")?)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub fn workspace_read_tools(root: impl AsRef<Path>) -> Result<ToolRegistry<'static>, ToolError> {
    let root = std::fs::canonicalize(root.as_ref()).map_err(ToolError::failed)?;
    if !root.is_dir() {
        return Err(ToolError::failed(
            "workspace root must be an existing directory",
        ));
    }
    let mut tools = ToolRegistry::default();
    let read_root = root.clone();
    tools.register(tool(
        "read",
        "Read a workspace text or image file. Text is capped at 2000 lines or 50 KiB, with a continuation hint. Images (PNG/JPEG/GIF/WebP) return inline, resized to at most 2000 pixels per side. Directories return a bounded listing and guidance to choose a file. Binary files return an ordinary tool error.",
        move |args: ReadArguments| {
            let root = read_root.clone();
            async move {
                let path = resolve(&root, &args.path)?;
                files::read::read(&path, &args.path, args.offset, args.limit)
                    .await.map_err(ToolError::failed)?.into_tool_output()
            }
        },
    ).map_err(ToolError::failed)?).map_err(ToolError::failed)?;
    tools.register(tool(
        "glob",
        "List workspace files matching a glob pattern (**/*/?/[...]), relative to the scope. Respects .gitignore, skips hidden entries and symlinks, and caps results at 1000. Narrow the pattern when truncated.",
        move |args: GlobArguments| {
            let root = root.clone();
            async move {
                let path = resolve(&root, &args.path)?;
                files::glob::glob(&path, &args.path, &args.pattern)
                    .map_err(ToolError::failed)?.into_tool_output()
            }
        },
    ).map_err(ToolError::failed)?).map_err(ToolError::failed)?;
    Ok(tools)
}

fn resolve(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    files::validate_input(input).map_err(ToolError::failed)?;
    files::path::resolve_workspace_path(root, Path::new(input)).map_err(ToolError::failed)
}

#[derive(Deserialize, JsonSchema)]
struct ReadArguments {
    /// File path inside the workspace; relative paths start at its root.
    path: String,
    /// Starting line (1-indexed).
    offset: Option<usize>,
    /// Maximum lines to read, also bounded by the built-in output caps.
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct GlobArguments {
    pattern: String,
    /// Directory inside the workspace; defaults to its root.
    #[serde(default = "default_scope")]
    path: String,
}

fn default_scope() -> String {
    ".".into()
}
