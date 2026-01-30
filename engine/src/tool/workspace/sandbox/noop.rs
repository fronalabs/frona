use std::process::Command;

use crate::error::AppError;

use super::{Sandbox, SandboxConfig};

pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
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
