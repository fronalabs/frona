use std::process::Command;

use crate::Error;

use super::{SandboxConfig, SandboxDriver};

pub struct NoopDriver;

impl SandboxDriver for NoopDriver {
    fn driver_id(&self) -> &'static str {
        "disabled"
    }

    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, Error> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.current_dir(&config.workspace_dir);
        Ok(cmd)
    }
}
