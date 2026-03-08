pub mod sandbox;

use std::path::{Path, PathBuf};

use crate::core::error::AppError;

use self::sandbox::{SandboxConfig, SandboxOutput, create_sandbox, execute_sandboxed};

pub struct WorkspaceManager {
    base_path: PathBuf,
    sandbox_disabled: bool,
}

impl WorkspaceManager {
    pub fn new(base_path: impl Into<PathBuf>, sandbox_disabled: bool) -> Self {
        Self {
            base_path: base_path.into(),
            sandbox_disabled,
        }
    }

    pub fn get_workspace(
        &self,
        agent_id: &str,
        network_access: bool,
        allowed_network_destinations: Vec<String>,
    ) -> Workspace {
        let sanitized = agent_id.replace(['/', '\\', ':', '\0'], "_");
        let path = self.base_path.join(&sanitized);
        Workspace {
            path,
            sandbox: create_sandbox(self.sandbox_disabled),
            network_access,
            allowed_network_destinations,
            skill_dirs: Vec::new(),
            extra_env_vars: Vec::new(),
        }
    }
}

pub struct Workspace {
    path: PathBuf,
    sandbox: Box<dyn sandbox::Sandbox>,
    network_access: bool,
    allowed_network_destinations: Vec<String>,
    skill_dirs: Vec<(String, String)>,
    extra_env_vars: Vec<(String, String)>,
}

impl Workspace {
    pub fn with_skill_dirs(mut self, skill_dirs: Vec<(String, String)>) -> Self {
        self.skill_dirs = skill_dirs;
        self
    }

    pub fn with_extra_env_vars(mut self, vars: Vec<(String, String)>) -> Self {
        self.extra_env_vars = vars;
        self
    }
}

impl Workspace {
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
                    "Failed to create workspace dir {}: {e}",
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
        stdin: Option<&str>,
        timeout_secs: u64,
    ) -> Result<SandboxOutput, AppError> {
        self.setup()?;

        let canonical_path = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());

        let mut additional_read_paths = Vec::new();
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

        execute_sandboxed(&*self.sandbox, program, &resolved_refs, stdin, &config).await
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

    fn temp_workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join("frona_test_workspace").join(name);
        Workspace {
            path: dir,
            sandbox: create_sandbox(false),
            network_access: false,
            allowed_network_destinations: Vec::new(),
            skill_dirs: Vec::new(),
            extra_env_vars: Vec::new(),
        }
    }

    #[test]
    fn test_setup_creates_venv() {
        if !python3_available() {
            eprintln!("python3 not found, skipping");
            return;
        }
        let ws = temp_workspace(&format!("setup_venv_{}", uuid::Uuid::new_v4()));
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

        let ws = Workspace {
            path: rel_path.clone(),
            sandbox: create_sandbox(false),
            network_access: false,
            allowed_network_destinations: Vec::new(),
            skill_dirs: Vec::new(),
            extra_env_vars: Vec::new(),
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
        let ws = temp_workspace(&format!("home_env_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        let result = ws
            .execute(
                "bash",
                &["-c", "echo $HOME; echo $XDG_CONFIG_HOME; echo $XDG_CACHE_HOME"],
                None,
                10,
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
        let ws = temp_workspace(&format!("env_leak_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        let result = ws
            .execute("bash", &["-c", "echo $FRONA_TEST_SECRET"], None, 10)
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
        let ws = temp_workspace(&format!("setup_idempotent_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        ws.setup().unwrap();
        ws.setup().unwrap();

        assert!(ws.venv_path().exists());

        let _ = std::fs::remove_dir_all(&ws.path);
    }
}
