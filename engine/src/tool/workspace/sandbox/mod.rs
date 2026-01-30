pub mod linux;
pub mod macos;
pub mod noop;

use std::process::Command;

use crate::error::AppError;

pub struct SandboxConfig {
    pub workspace_dir: String,
    pub network_access: bool,
    pub allowed_network_destinations: Vec<String>,
    pub additional_read_paths: Vec<String>,
    pub additional_path_dirs: Vec<String>,
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            workspace_dir: String::new(),
            network_access: false,
            allowed_network_destinations: Vec::new(),
            additional_read_paths: Vec::new(),
            additional_path_dirs: Vec::new(),
            timeout_secs: 30,
            max_output_bytes: 512 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

pub trait Sandbox: Send + Sync {
    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, AppError>;
}

pub fn create_sandbox() -> Box<dyn Sandbox> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOsSandbox)
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LandlockSandbox)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Box::new(noop::NoopSandbox)
    }
}

pub async fn execute_sandboxed(
    sandbox: &dyn Sandbox,
    program: &str,
    args: &[&str],
    stdin_data: Option<&str>,
    config: &SandboxConfig,
) -> Result<SandboxOutput, AppError> {
    let mut cmd = sandbox.sandboxed_command(program, args, config)?;

    if !config.additional_path_dirs.is_empty() {
        let existing = std::env::var("PATH").unwrap_or_default();
        let extra = config.additional_path_dirs.join(":");
        cmd.env("PATH", format!("{extra}:{existing}"));
    }

    if stdin_data.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let timeout = std::time::Duration::from_secs(config.timeout_secs);
    let max_bytes = config.max_output_bytes;
    let stdin_owned = stdin_data.map(|s| s.to_string());

    tokio::task::spawn_blocking(move || {
        let mut child = cmd
            .spawn()
            .map_err(|e| AppError::Tool(format!("Failed to spawn process: {e}")))?;

        if let Some(data) = stdin_owned {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data.as_bytes());
                drop(stdin);
            }
        }

        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stdout_bytes = Vec::new();
                    let mut stderr_bytes = Vec::new();
                    use std::io::Read;
                    if let Some(mut out) = child.stdout.take() {
                        let _ = out.read_to_end(&mut stdout_bytes);
                    }
                    if let Some(mut err) = child.stderr.take() {
                        let _ = err.read_to_end(&mut stderr_bytes);
                    }

                    return Ok(SandboxOutput {
                        stdout: truncate_output(
                            String::from_utf8_lossy(&stdout_bytes).into_owned(),
                            max_bytes,
                        ),
                        stderr: truncate_output(
                            String::from_utf8_lossy(&stderr_bytes).into_owned(),
                            max_bytes,
                        ),
                        exit_code: status.code(),
                        timed_out: false,
                    });
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(SandboxOutput {
                            stdout: String::new(),
                            stderr: "Process timed out".to_string(),
                            exit_code: None,
                            timed_out: true,
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(AppError::Tool(format!("Failed to wait on process: {e}")));
                }
            }
        }
    })
    .await
    .map_err(|e| AppError::Tool(format!("Task join error: {e}")))?
}

fn truncate_output(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s
    } else {
        let truncated = &s.as_bytes()[..max_bytes];
        let valid = String::from_utf8_lossy(truncated);
        format!("{valid}\n... (output truncated at {max_bytes} bytes)")
    }
}
