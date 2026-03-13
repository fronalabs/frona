pub mod linux;
pub mod macos;
pub mod noop;

use std::process::Command;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    pub cancelled: bool,
}

pub trait SandboxDriver: Send + Sync {
    fn sandboxed_command(
        &self,
        program: &str,
        args: &[&str],
        config: &SandboxConfig,
    ) -> Result<Command, AppError>;
}

pub fn create_driver(disabled: bool) -> Box<dyn SandboxDriver> {
    if disabled {
        return Box::new(noop::NoopDriver);
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosDriver)
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LandlockDriver)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Box::new(noop::NoopDriver)
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
    let sandbox = create_driver(false);
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
        &config,
        None,
        None,
        None,
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
        &config,
        None,
        None,
        None,
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
    sandbox: &dyn SandboxDriver,
    program: &str,
    args: &[&str],
    config: &SandboxConfig,
    on_stdout: Option<mpsc::Sender<String>>,
    stdin_rx: Option<mpsc::Receiver<String>>,
    cancel_token: Option<CancellationToken>,
) -> Result<SandboxOutput, AppError> {
    let mut std_cmd = sandbox.sandboxed_command(program, args, config)?;

    std_cmd.env_clear();

    const PASSTHROUGH_VARS: &[&str] = &[
        "TERM", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "USER", "LOGNAME", "TMPDIR", "SHELL",
    ];

    for key in PASSTHROUGH_VARS {
        if let Ok(val) = std::env::var(key) {
            std_cmd.env(key, val);
        }
    }

    {
        let existing = std::env::var("PATH").unwrap_or_default();
        if config.additional_path_dirs.is_empty() {
            std_cmd.env("PATH", existing);
        } else {
            let extra = config.additional_path_dirs.join(":");
            std_cmd.env("PATH", format!("{extra}:{existing}"));
        }
    }

    for (key, value) in &config.env_vars {
        std_cmd.env(key, value);
    }

    if stdin_rx.is_some() {
        std_cmd.stdin(std::process::Stdio::piped());
    }
    std_cmd.stdout(std::process::Stdio::piped());
    std_cmd.stderr(std::process::Stdio::piped());

    let mut cmd = tokio::process::Command::from(std_cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::Tool(format!("Failed to spawn process: {e}")))?;

    if let Some(mut rx) = stdin_rx {
        let mut stdin_pipe = child.stdin.take().expect("stdin pipe was configured");
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(data) = rx.recv().await {
                if stdin_pipe.write_all(data.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }

    let stdout_pipe = child
        .stdout
        .take()
        .expect("stdout pipe was configured");
    let mut stdout_reader =
        tokio::io::BufReader::new(stdout_pipe).lines();

    let timeout = std::time::Duration::from_secs(config.timeout_secs);
    let max_bytes = config.max_output_bytes;

    let mut stdout_lines: Vec<String> = Vec::new();
    let mut timed_out = false;
    let mut cancelled = false;

    use tokio::io::AsyncBufReadExt;

    loop {
        tokio::select! {
            line = stdout_reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(tx) = &on_stdout {
                            let _ = tx.send(line.clone()).await;
                        }
                        stdout_lines.push(line);
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep(timeout) => {
                timed_out = true;
                let _ = child.kill().await;
                break;
            }
            _ = async {
                if let Some(token) = &cancel_token {
                    token.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                cancelled = true;
                let _ = child.kill().await;
                break;
            }
        }
    }

    let status = child.wait().await.ok();

    let mut stderr_bytes = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        use tokio::io::AsyncReadExt;
        let _ = err.read_to_end(&mut stderr_bytes).await;
    }

    let stdout = stdout_lines.join("\n");
    let stdout = if !stdout.is_empty() && !stdout_lines.is_empty() {
        format!("{stdout}\n")
    } else {
        stdout
    };

    let stderr = if timed_out && stderr_bytes.is_empty() {
        "Process timed out".to_string()
    } else {
        String::from_utf8_lossy(&stderr_bytes).into_owned()
    };

    Ok(SandboxOutput {
        stdout: truncate_output(stdout, max_bytes),
        stderr: truncate_output(stderr, max_bytes),
        exit_code: if timed_out || cancelled {
            None
        } else {
            status.and_then(|s| s.code())
        },
        timed_out,
        cancelled,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sandbox() -> Box<dyn SandboxDriver> {
        create_driver(false)
    }

    fn test_config(timeout_secs: u64) -> SandboxConfig {
        let dir = std::env::temp_dir()
            .join("frona_sandbox_test")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        SandboxConfig {
            workspace_dir: dir.to_string_lossy().into_owned(),
            timeout_secs,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_execute_sandboxed_basic() {
        let sandbox = test_sandbox();
        let config = test_config(10);

        let output = execute_sandboxed(&*sandbox, "echo", &["hello"], &config, None, None, None)
            .await
            .unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("hello"));
        assert!(!output.timed_out);
        assert!(!output.cancelled);
    }

    #[tokio::test]
    async fn test_execute_sandboxed_streaming_stdout() {
        let sandbox = test_sandbox();
        let config = test_config(10);
        let (tx, mut rx) = mpsc::channel(16);

        let output = execute_sandboxed(
            &*sandbox,
            "bash",
            &["-c", "echo line1; echo line2; echo line3"],
            &config,
            Some(tx),
            None,
            None,
        )
        .await
        .unwrap();

        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line1");
        assert_eq!(lines[1], "line2");
        assert_eq!(lines[2], "line3");
        assert!(output.stdout.contains("line1"));
        assert!(output.stdout.contains("line3"));
    }

    #[tokio::test]
    async fn test_execute_sandboxed_streaming_slow_process() {
        let sandbox = test_sandbox();
        let config = test_config(10);
        let (tx, mut rx) = mpsc::channel(16);

        let handle = tokio::spawn(async move {
            let mut timestamps = Vec::new();
            while let Some(_line) = rx.recv().await {
                timestamps.push(tokio::time::Instant::now());
            }
            timestamps
        });

        let output = execute_sandboxed(
            &*sandbox,
            "bash",
            &["-c", "for i in 1 2 3; do echo $i; sleep 0.1; done"],
            &config,
            Some(tx),
            None,
            None,
        )
        .await
        .unwrap();

        let timestamps = handle.await.unwrap();
        assert_eq!(timestamps.len(), 3);
        assert_eq!(output.exit_code, Some(0));

        let spread = timestamps.last().unwrap().duration_since(*timestamps.first().unwrap());
        assert!(
            spread >= std::time::Duration::from_millis(150),
            "lines should arrive incrementally, spread was {:?}",
            spread
        );
    }

    #[tokio::test]
    async fn test_execute_sandboxed_timeout() {
        let sandbox = test_sandbox();
        let config = test_config(1);

        let output = execute_sandboxed(
            &*sandbox,
            "sleep",
            &["60"],
            &config,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(output.timed_out);
        assert!(!output.cancelled);
    }

    #[tokio::test]
    async fn test_execute_sandboxed_timeout_returns_partial_output() {
        let sandbox = test_sandbox();
        let config = test_config(1);
        let (tx, mut rx) = mpsc::channel(16);

        let output = execute_sandboxed(
            &*sandbox,
            "bash",
            &["-c", "echo partial; sleep 60"],
            &config,
            Some(tx),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(output.timed_out);
        assert!(output.stdout.contains("partial"));

        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        assert!(lines.iter().any(|l| l == "partial"));
    }

    #[tokio::test]
    async fn test_execute_sandboxed_cancel() {
        let sandbox = test_sandbox();
        let config = test_config(60);
        let token = CancellationToken::new();
        let token_clone = token.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            token_clone.cancel();
        });

        let output = execute_sandboxed(
            &*sandbox,
            "sleep",
            &["60"],
            &config,
            None,
            None,
            Some(token),
        )
        .await
        .unwrap();

        assert!(output.cancelled);
        assert!(!output.timed_out);
    }

    #[tokio::test]
    async fn test_execute_sandboxed_cancel_returns_partial_output() {
        let sandbox = test_sandbox();
        let config = test_config(60);
        let token = CancellationToken::new();
        let token_clone = token.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            token_clone.cancel();
        });

        let output = execute_sandboxed(
            &*sandbox,
            "bash",
            &["-c", "echo before_cancel; sleep 60"],
            &config,
            None,
            None,
            Some(token),
        )
        .await
        .unwrap();

        assert!(output.cancelled);
        assert!(output.stdout.contains("before_cancel"));
    }

    #[tokio::test]
    async fn test_execute_sandboxed_stdin_channel() {
        let sandbox = test_sandbox();
        let config = test_config(10);
        let (tx, rx) = mpsc::channel(16);

        tx.send("hello\n".to_string()).await.unwrap();
        drop(tx);

        let output = execute_sandboxed(
            &*sandbox,
            "cat",
            &[],
            &config,
            None,
            Some(rx),
            None,
        )
        .await
        .unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_execute_sandboxed_stdin_then_closes() {
        let sandbox = test_sandbox();
        let config = test_config(10);
        let (tx, rx) = mpsc::channel(16);

        tx.send("first line\n".to_string()).await.unwrap();
        drop(tx);

        let output = execute_sandboxed(
            &*sandbox,
            "head",
            &["-1"],
            &config,
            None,
            Some(rx),
            None,
        )
        .await
        .unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("first line"));
    }

    #[tokio::test]
    async fn test_execute_sandboxed_output_truncation() {
        let sandbox = test_sandbox();
        let mut config = test_config(10);
        config.max_output_bytes = 50;

        let output = execute_sandboxed(
            &*sandbox,
            "bash",
            &["-c", "yes | head -100"],
            &config,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(output.stdout.contains("output truncated at 50 bytes"));
    }

    #[tokio::test]
    async fn test_execute_sandboxed_nonzero_exit() {
        let sandbox = test_sandbox();
        let config = test_config(10);

        let output = execute_sandboxed(
            &*sandbox,
            "bash",
            &["-c", "exit 42"],
            &config,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(output.exit_code, Some(42));
        assert!(!output.timed_out);
        assert!(!output.cancelled);
    }
}
