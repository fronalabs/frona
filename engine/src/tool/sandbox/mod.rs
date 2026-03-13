pub mod driver;

use std::path::{Path, PathBuf};

use crate::core::error::AppError;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::driver::{SandboxConfig, SandboxOutput, create_driver, execute_sandboxed};

pub struct SandboxManager {
    base_path: PathBuf,
    sandbox_disabled: bool,
    shared_read_paths: Vec<String>,
}

impl SandboxManager {
    pub fn new(base_path: impl Into<PathBuf>, sandbox_disabled: bool) -> Self {
        Self {
            base_path: base_path.into(),
            sandbox_disabled,
            shared_read_paths: Vec::new(),
        }
    }

    pub fn with_shared_read_paths(mut self, paths: Vec<String>) -> Self {
        self.shared_read_paths = paths;
        self
    }

    pub fn get_sandbox(
        &self,
        agent_id: &str,
        network_access: bool,
        allowed_network_destinations: Vec<String>,
    ) -> Sandbox {
        let sanitized = agent_id.replace(['/', '\\', ':', '\0'], "_");
        let path = self.base_path.join(&sanitized);
        Sandbox {
            path,
            sandbox: create_driver(self.sandbox_disabled),
            network_access,
            allowed_network_destinations,
            skill_dirs: Vec::new(),
            extra_env_vars: Vec::new(),
            shared_read_paths: self.shared_read_paths.clone(),
        }
    }
}

pub struct Sandbox {
    path: PathBuf,
    sandbox: Box<dyn driver::SandboxDriver>,
    network_access: bool,
    allowed_network_destinations: Vec<String>,
    skill_dirs: Vec<(String, String)>,
    extra_env_vars: Vec<(String, String)>,
    shared_read_paths: Vec<String>,
}

impl Sandbox {
    pub fn with_skill_dirs(mut self, skill_dirs: Vec<(String, String)>) -> Self {
        self.skill_dirs = skill_dirs;
        self
    }

    pub fn with_extra_env_vars(mut self, vars: Vec<(String, String)>) -> Self {
        self.extra_env_vars = vars;
        self
    }
}

impl Sandbox {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn venv_path(&self) -> PathBuf {
        self.path.join(".venv")
    }

    pub fn setup(&self) -> Result<(), AppError> {
        if !self.path.exists() {
            std::fs::create_dir_all(&self.path).map_err(|e| {
                AppError::Tool(format!(
                    "Failed to create sandbox dir {}: {e}",
                    self.path.display()
                ))
            })?;
        }
        self.setup_venv();
        Ok(())
    }

    fn setup_venv(&self) {
        let venv = self.venv_path();
        if venv.exists() {
            return;
        }

        let result = std::process::Command::new("python3")
            .args(["-m", "venv", "--system-site-packages", ".venv"])
            .current_dir(&self.path)
            .output();

        match result {
            Ok(output) if output.status.success() => {
                tracing::info!(path = %venv.display(), "Created Python venv");
            }
            Ok(output) => {
                tracing::warn!(
                    stderr = String::from_utf8_lossy(&output.stderr).as_ref(),
                    "Failed to create Python venv"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "python3 not available, skipping venv creation");
            }
        }
    }

    pub async fn execute(
        &self,
        program: &str,
        args: &[&str],
        timeout_secs: u64,
        on_stdout: Option<mpsc::Sender<String>>,
        stdin_rx: Option<mpsc::Receiver<String>>,
        cancel_token: Option<CancellationToken>,
    ) -> Result<SandboxOutput, AppError> {
        self.setup()?;

        let canonical_path = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());

        let mut additional_read_paths = self.shared_read_paths.clone();
        let mut additional_path_dirs = Vec::new();
        let mut env_vars = Vec::new();
        let mut resolved_args: Vec<String> = Vec::new();

        let venv_bin = canonical_path.join(".venv").join("bin");
        if venv_bin.exists() {
            additional_path_dirs.push(venv_bin.to_string_lossy().into_owned());
            env_vars.push((
                "VIRTUAL_ENV".to_string(),
                canonical_path.join(".venv").to_string_lossy().into_owned(),
            ));
        }

        // Set HOME to workspace so tools write config inside the sandbox
        env_vars.push(("HOME".to_string(), canonical_path.to_string_lossy().into_owned()));
        env_vars.push(("XDG_CONFIG_HOME".to_string(), canonical_path.join(".config").to_string_lossy().into_owned()));
        env_vars.push(("XDG_CACHE_HOME".to_string(), canonical_path.join(".cache").to_string_lossy().into_owned()));

        for arg in args {
            let mut resolved = arg.to_string();
            for (prefix, abs_dir) in &self.skill_dirs {
                if resolved.contains(prefix) {
                    resolved = resolved.replace(prefix, abs_dir);
                }
                if !additional_read_paths.contains(abs_dir) {
                    additional_read_paths.push(abs_dir.clone());
                }
                if !additional_path_dirs.contains(abs_dir) {
                    additional_path_dirs.push(abs_dir.clone());
                }
            }
            resolved_args.push(resolved);
        }

        let resolved_refs: Vec<&str> = resolved_args.iter().map(|s| s.as_str()).collect();

        env_vars.extend(self.extra_env_vars.clone());

        let config = SandboxConfig {
            workspace_dir: canonical_path.to_string_lossy().into_owned(),
            network_access: self.network_access,
            allowed_network_destinations: self.allowed_network_destinations.clone(),
            additional_read_paths,
            additional_path_dirs,
            env_vars,
            timeout_secs,
            ..Default::default()
        };

        execute_sandboxed(
            &*self.sandbox,
            program,
            &resolved_refs,
            &config,
            on_stdout,
            stdin_rx,
            cancel_token,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn temp_sandbox(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join("frona_test_workspace").join(name);
        Sandbox {
            path: dir,
            sandbox: create_driver(false),
            network_access: false,
            allowed_network_destinations: Vec::new(),
            skill_dirs: Vec::new(),
            extra_env_vars: Vec::new(),
            shared_read_paths: Vec::new(),
        }
    }

    #[test]
    fn test_setup_creates_venv() {
        if !python3_available() {
            eprintln!("python3 not found, skipping");
            return;
        }
        let ws = temp_sandbox(&format!("setup_venv_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        ws.setup().unwrap();

        assert!(ws.path.exists());
        assert!(ws.venv_path().exists());
        assert!(ws.venv_path().join("bin").join("python3").exists());

        let _ = std::fs::remove_dir_all(&ws.path);
    }

    #[test]
    fn test_setup_venv_with_relative_path() {
        if !python3_available() {
            eprintln!("python3 not found, skipping");
            return;
        }
        let name = format!("relative_venv_{}", uuid::Uuid::new_v4());
        let rel_base = PathBuf::from("target").join("test_workspaces");
        let rel_path = rel_base.join(&name);
        let _ = std::fs::remove_dir_all(&rel_path);

        let ws = Sandbox {
            path: rel_path.clone(),
            sandbox: create_driver(false),
            network_access: false,
            allowed_network_destinations: Vec::new(),
            skill_dirs: Vec::new(),
            extra_env_vars: Vec::new(),
            shared_read_paths: Vec::new(),
        };
        ws.setup().unwrap();

        assert!(rel_path.join(".venv").exists(), "venv should exist at the workspace root");
        assert!(
            rel_path.join(".venv").join("bin").join("python3").exists(),
            "venv should contain bin/python3"
        );
        assert!(
            !rel_path.join("target").exists(),
            "no nested dirs should be created inside workspace"
        );

        let _ = std::fs::remove_dir_all(&rel_path);
    }

    #[tokio::test]
    async fn test_execute_sets_home_and_xdg_env_vars() {
        let ws = temp_sandbox(&format!("home_env_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        let result = ws
            .execute(
                "bash",
                &["-c", "echo $HOME; echo $XDG_CONFIG_HOME; echo $XDG_CACHE_HOME"],
                10,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let canonical = std::fs::canonicalize(&ws.path).unwrap();
        let lines: Vec<&str> = result.stdout.trim().lines().collect();
        assert_eq!(lines.len(), 3, "expected 3 lines, got: {:?}", lines);
        assert_eq!(lines[0], canonical.to_string_lossy());
        assert_eq!(lines[1], canonical.join(".config").to_string_lossy().as_ref());
        assert_eq!(lines[2], canonical.join(".cache").to_string_lossy().as_ref());

        let _ = std::fs::remove_dir_all(&ws.path);
    }

    #[tokio::test]
    async fn test_execute_does_not_leak_parent_env() {
        unsafe { std::env::set_var("FRONA_TEST_SECRET", "leaked") };
        let ws = temp_sandbox(&format!("env_leak_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        let result = ws
            .execute("bash", &["-c", "echo $FRONA_TEST_SECRET"], 10, None, None, None)
            .await
            .unwrap();

        assert_eq!(result.stdout.trim(), "", "parent env vars must not leak into sandbox");

        unsafe { std::env::remove_var("FRONA_TEST_SECRET") };
        let _ = std::fs::remove_dir_all(&ws.path);
    }

    #[test]
    fn test_setup_idempotent() {
        if !python3_available() {
            eprintln!("python3 not found, skipping");
            return;
        }
        let ws = temp_sandbox(&format!("setup_idempotent_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        ws.setup().unwrap();
        ws.setup().unwrap();

        assert!(ws.venv_path().exists());

        let _ = std::fs::remove_dir_all(&ws.path);
    }

    #[tokio::test]
    async fn test_execute_async_streaming() {
        let ws = temp_sandbox(&format!("streaming_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);
        let (tx, mut rx) = mpsc::channel(16);

        let result = ws
            .execute(
                "bash",
                &["-c", "echo hello; echo world"],
                10,
                Some(tx),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("hello"));
        assert!(result.stdout.contains("world"));

        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world");

        let _ = std::fs::remove_dir_all(&ws.path);
    }
}
