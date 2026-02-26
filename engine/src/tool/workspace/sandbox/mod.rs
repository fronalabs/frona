pub mod linux;
pub mod macos;
pub mod noop;

use std::process::Command;

/// Specific /etc paths allowed for read access.
/// We intentionally exclude /etc/passwd, /etc/shadow, /etc/group,
/// /etc/ssh/, and other sensitive files.
pub const ETC_READ_ALLOWLIST: &[&str] = &[
    // Dynamic linker
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    // DNS / networking
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/gai.conf",
    "/etc/protocols",
    "/etc/services",
    // SSL / TLS certificates
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/pki",
    // Timezone
    "/etc/localtime",
    "/etc/timezone",
    "/etc/zoneinfo",
    // Locale
    "/etc/locale.conf",
    "/etc/default",
    // Shell config (needed by bash -c)
    "/etc/bash.bashrc",
    "/etc/profile",
    "/etc/profile.d",
    "/etc/inputrc",
    "/etc/environment",
    // Fonts (needed by matplotlib, ImageMagick, etc.)
    "/etc/fonts",
    // Misc
    "/etc/hostname",
    "/etc/machine-id",
    "/etc/mime.types",
    "/etc/alternatives",
    "/etc/login.defs",
    // OS identification (needed by pip/distro)
    "/etc/os-release",
    "/etc/lsb-release",
    "/etc/debian_version",
    // Language runtimes
    "/etc/python3",
    "/etc/pip.conf",
];

use crate::core::error::AppError;

pub struct SandboxConfig {
    pub workspace_dir: String,
    pub network_access: bool,
    pub allowed_network_destinations: Vec<String>,
    pub additional_read_paths: Vec<String>,
    pub additional_path_dirs: Vec<String>,
    pub env_vars: Vec<(String, String)>,
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
            env_vars: Vec::new(),
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

pub fn create_sandbox(disabled: bool) -> Box<dyn Sandbox> {
    if disabled {
        return Box::new(noop::NoopSandbox);
    }
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

pub fn verify_sandbox(workspace_base: &str, disabled: bool) -> Result<(), String> {
    if disabled {
        tracing::warn!("Sandbox disabled by FRONA_SERVER_SANDBOX_DISABLED env var");
        return Ok(());
    }

    let probe_dir = std::path::Path::new(workspace_base).join(".sandbox_probe");
    if let Err(e) = std::fs::create_dir_all(&probe_dir) {
        return Err(format!(
            "Cannot create probe directory {}: {e}",
            probe_dir.display()
        ));
    }

    let rt = tokio::runtime::Handle::try_current();
    let result = match rt {
        Ok(handle) => {
            std::thread::scope(|s| {
                s.spawn(|| {
                    handle.block_on(run_sandbox_probe(&probe_dir))
                }).join().unwrap()
            })
        }
        Err(_) => {
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| format!("Failed to create runtime: {e}"))?;
            rt.block_on(run_sandbox_probe(&probe_dir))
        }
    };

    let _ = std::fs::remove_dir_all(&probe_dir);
    result
}

async fn run_sandbox_probe(probe_dir: &std::path::Path) -> Result<(), String> {
    let sandbox = create_sandbox(false);
    let canonical = std::fs::canonicalize(probe_dir)
        .unwrap_or_else(|_| probe_dir.to_path_buf());

    let config = SandboxConfig {
        workspace_dir: canonical.to_string_lossy().into_owned(),
        timeout_secs: 10,
        ..Default::default()
    };

    let write_ok = execute_sandboxed(
        &*sandbox,
        "bash",
        &["-c", "echo ok > probe.txt && cat probe.txt"],
        None,
        &config,
    )
    .await
    .map_err(|e| format!("Sandbox probe spawn failed: {e}"))?;

    if write_ok.exit_code != Some(0) {
        return Err(format!(
            "Sandbox blocks writes to workspace — filesystem may not support sandboxing. \
             stderr: {}",
            write_ok.stderr
        ));
    }

    let forbidden_path = format!(
        "/root/.sandbox_probe_{}",
        std::process::id()
    );
    let forbidden_cmd = format!("echo hacked > {forbidden_path}");
    let forbidden_result = execute_sandboxed(
        &*sandbox,
        "bash",
        &["-c", &forbidden_cmd],
        None,
        &config,
    )
    .await
    .map_err(|e| format!("Sandbox probe spawn failed: {e}"))?;

    if forbidden_result.exit_code == Some(0) {
        return Err(
            "Sandbox is not enforcing restrictions — writes to forbidden paths are allowed"
                .to_string(),
        );
    }

    tracing::info!("Sandbox verified: enforcement is active");
    Ok(())
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

    for (key, value) in &config.env_vars {
        cmd.env(key, value);
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
