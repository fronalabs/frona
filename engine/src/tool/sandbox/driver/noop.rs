use std::process::Command;

use crate::core::error::AppError;

use super::{SandboxDriver, SandboxConfig};

pub struct NoopDriver;

impl SandboxDriver for NoopDriver {
    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, AppError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.current_dir(&config.workspace_dir);
        Ok(cmd)
    }
}
