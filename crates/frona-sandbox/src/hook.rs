use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

/// Errors supplied by trusted execution hooks.
pub type HookError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type HookFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ExecutionLease, HookError>> + Send + 'a>>;

/// The command being prepared. Values are supplied by the sandbox caller.
pub struct ExecutionContext<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub workspace: &'a Path,
    pub working_directory: &'a Path,
    pub owner_id: &'a str,
}

/// Trusted host preparation, invoked afresh for each execute/spawn operation.
/// Dropping preparation cancels it; acquired resources should use RAII cleanup.
pub trait ExecutionHook: Send + Sync {
    fn prepare<'a>(&'a self, context: &'a ExecutionContext<'a>) -> HookFuture<'a>;
}

/// Per-execution additions and resources retained until the child is reaped or
/// dropped. Cleanup uses Drop and must not require async work. Hook environment
/// values override existing values; file grants remain subject to backend denies.
#[derive(Default)]
pub struct ExecutionLease {
    pub(crate) env: Vec<(String, String)>,
    pub(crate) read_files: Vec<PathBuf>,
    retained: Vec<Box<dyn Send + Sync>>,
}

impl ExecutionLease {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }
    pub fn read_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.read_files.push(path.into());
        self
    }
    pub fn retain<T: Send + Sync + 'static>(mut self, resource: T) -> Self {
        self.retained.push(Box::new(resource));
        self
    }
}
