pub mod sandbox;

use std::path::{Path, PathBuf};

use crate::error::AppError;

use self::sandbox::{SandboxConfig, SandboxOutput, create_sandbox, execute_sandboxed};

pub struct WorkspaceManager {
    base_path: PathBuf,
}

impl WorkspaceManager {
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
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
            sandbox: create_sandbox(),
            network_access,
            allowed_network_destinations,
            skill_dirs: Vec::new(),
        }
    }
}

pub struct Workspace {
    path: PathBuf,
    sandbox: Box<dyn sandbox::Sandbox>,
    network_access: bool,
    allowed_network_destinations: Vec<String>,
    skill_dirs: Vec<(String, String)>,
}

impl Workspace {
    pub fn with_skill_dirs(mut self, skill_dirs: Vec<(String, String)>) -> Self {
        self.skill_dirs = skill_dirs;
        self
    }
}

impl Workspace {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure(&self) -> Result<(), AppError> {
        if !self.path.exists() {
            std::fs::create_dir_all(&self.path).map_err(|e| {
                AppError::Tool(format!(
                    "Failed to create workspace dir {}: {e}",
                    self.path.display()
                ))
            })?;
        }
        Ok(())
    }

    pub async fn execute(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
        timeout_secs: u64,
    ) -> Result<SandboxOutput, AppError> {
        self.ensure()?;

        let mut additional_read_paths = Vec::new();
        let mut additional_path_dirs = Vec::new();
        let mut resolved_args: Vec<String> = Vec::new();

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

        let config = SandboxConfig {
            workspace_dir: self.path.to_string_lossy().into_owned(),
            network_access: self.network_access,
            allowed_network_destinations: self.allowed_network_destinations.clone(),
            additional_read_paths,
            additional_path_dirs,
            timeout_secs,
            ..Default::default()
        };

        execute_sandboxed(&*self.sandbox, program, &resolved_refs, stdin, &config).await
    }
}
