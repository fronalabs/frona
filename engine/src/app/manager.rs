use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::core::error::AppError;
use crate::tool::sandbox::driver::{SandboxConfig, SandboxDriver, create_driver};

use super::models::{AppManifest, HealthCheck};

pub struct ManagedProcess {
    pub child: tokio::process::Child,
    pub port: u16,
    pub agent_id: String,
    pub manifest: AppManifest,
    pub credential_env_vars: Vec<(String, String)>,
    pub restart_count: u32,
    pub consecutive_failures: u32,
}

pub struct AppManager {
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    allocated_ports: Arc<Mutex<HashSet<u16>>>,
    port_range: (u16, u16),
    workspaces_path: PathBuf,
    sandbox_disabled: bool,
    last_accessed: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
}

impl AppManager {
    pub fn new(
        workspaces_path: PathBuf,
        sandbox_disabled: bool,
        port_range_start: u16,
        port_range_end: u16,
    ) -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            allocated_ports: Arc::new(Mutex::new(HashSet::new())),
            port_range: (port_range_start, port_range_end),
            workspaces_path,
            sandbox_disabled,
            last_accessed: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start_app(
        &self,
        app_id: &str,
        agent_id: &str,
        command: &str,
        manifest: &AppManifest,
        credential_env_vars: Vec<(String, String)>,
    ) -> Result<(u16, u32), AppError> {
        let port = self.allocate_port().await?;
        let workspace_dir = self.workspace_path(agent_id);

        let sandbox = create_driver(self.sandbox_disabled);

        let child = self.spawn_process(
            &*sandbox,
            &workspace_dir,
            command,
            port,
            manifest,
            &credential_env_vars,
        )?;

        let pid = child.id().unwrap_or(0);

        let managed = ManagedProcess {
            child,
            port,
            agent_id: agent_id.to_string(),
            manifest: manifest.clone(),
            credential_env_vars,
            restart_count: 0,
            consecutive_failures: 0,
        };

        self.processes
            .lock()
            .await
            .insert(app_id.to_string(), managed);

        Ok((port, pid))
    }

    pub async fn stop_app(&self, app_id: &str) -> Result<(), AppError> {
        let mut processes = self.processes.lock().await;
        if let Some(mut proc) = processes.remove(app_id) {
            let port = proc.port;

            let _ = proc.child.kill().await;
            let _ = proc.child.wait().await;

            self.allocated_ports.lock().await.remove(&port);
        }
        Ok(())
    }

    pub async fn health_check(&self, port: u16, health_check: &HealthCheck) -> bool {
        let url = format!("http://127.0.0.1:{}{}", port, health_check.path);
        let timeout = std::time::Duration::from_secs(health_check.effective_timeout());

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build();

        let client = match client {
            Ok(c) => c,
            Err(_) => return false,
        };

        client.get(&url).send().await.is_ok_and(|r| r.status().is_success())
    }

    pub async fn is_process_alive(&self, app_id: &str) -> bool {
        let mut processes = self.processes.lock().await;
        if let Some(proc) = processes.get_mut(app_id) {
            matches!(proc.child.try_wait(), Ok(None))
        } else {
            false
        }
    }

    pub async fn restart_app(
        &self,
        app_id: &str,
        agent_id: &str,
    ) -> Result<Option<(u16, u32)>, AppError> {
        let (command, manifest, creds) = {
            let processes = self.processes.lock().await;
            match processes.get(app_id) {
                Some(proc) => (
                    proc.manifest.command.clone(),
                    proc.manifest.clone(),
                    proc.credential_env_vars.clone(),
                ),
                None => return Ok(None),
            }
        };

        self.stop_app(app_id).await?;

        if let Some(cmd) = command {
            let (port, pid) = self.start_app(app_id, agent_id, &cmd, &manifest, creds).await?;
            Ok(Some((port, pid)))
        } else {
            Ok(None)
        }
    }

    pub async fn try_restart_crashed(
        &self,
        app_id: &str,
        max_restarts: u32,
    ) -> Result<Option<(u16, u32)>, AppError> {
        let info = {
            let mut processes = self.processes.lock().await;
            if let Some(proc) = processes.get_mut(app_id) {
                if proc.restart_count >= max_restarts {
                    return Ok(None);
                }

                let should_restart = match proc.manifest.effective_restart_policy() {
                    "always" => true,
                    "on_failure" => {
                        match proc.child.try_wait() {
                            Ok(Some(status)) => !status.success(),
                            _ => true,
                        }
                    }
                    _ => false,
                };

                if !should_restart {
                    return Ok(None);
                }

                proc.restart_count += 1;
                Some((
                    proc.agent_id.clone(),
                    proc.manifest.command.clone(),
                    proc.manifest.clone(),
                    proc.credential_env_vars.clone(),
                ))
            } else {
                None
            }
        };

        if let Some((agent_id, command, manifest, creds)) = info {
            self.stop_app(app_id).await?;
            if let Some(cmd) = command {
                let (port, pid) =
                    self.start_app(app_id, &agent_id, &cmd, &manifest, creds).await?;
                return Ok(Some((port, pid)));
            }
        }

        Ok(None)
    }

    pub async fn record_access(&self, app_id: &str) {
        self.last_accessed
            .lock()
            .await
            .insert(app_id.to_string(), Utc::now());
    }

    pub async fn get_last_accessed(&self, app_id: &str) -> Option<DateTime<Utc>> {
        self.last_accessed.lock().await.get(app_id).copied()
    }

    pub async fn flush_access_times(&self) -> HashMap<String, DateTime<Utc>> {
        let mut map = self.last_accessed.lock().await;
        std::mem::take(&mut *map)
    }

    pub async fn get_process_port(&self, app_id: &str) -> Option<u16> {
        self.processes.lock().await.get(app_id).map(|p| p.port)
    }

    pub async fn has_process(&self, app_id: &str) -> bool {
        self.processes.lock().await.contains_key(app_id)
    }

    pub async fn get_managed_app_ids(&self) -> Vec<String> {
        self.processes.lock().await.keys().cloned().collect()
    }

    async fn allocate_port(&self) -> Result<u16, AppError> {
        let mut ports = self.allocated_ports.lock().await;
        for port in self.port_range.0..self.port_range.1 {
            if !ports.contains(&port) {
                ports.insert(port);
                return Ok(port);
            }
        }
        Err(AppError::Internal("No available ports in range".into()))
    }

    fn workspace_path(&self, agent_id: &str) -> String {
        let sanitized = agent_id.replace(['/', '\\', ':', '\0'], "_");
        let path = self.workspaces_path.join(sanitized);
        std::fs::canonicalize(&path)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn spawn_process(
        &self,
        sandbox: &dyn SandboxDriver,
        workspace_dir: &str,
        command: &str,
        port: u16,
        manifest: &AppManifest,
        credential_env_vars: &[(String, String)],
    ) -> Result<tokio::process::Child, AppError> {
        let mut allowed_destinations: Vec<String> = Vec::new();
        allowed_destinations.push(format!("127.0.0.1:{port}"));

        if let Some(dests) = &manifest.network_destinations {
            for dest in dests {
                allowed_destinations.push(format!("{}:{}", dest.host, dest.port));
            }
        }

        let base_path = format!("/apps/{}/", manifest.id);
        let mut env_vars = vec![
            ("PORT".to_string(), port.to_string()),
            ("BASE_PATH".to_string(), base_path),
        ];
        env_vars.extend(credential_env_vars.iter().cloned());

        let venv_bin = PathBuf::from(workspace_dir).join(".venv").join("bin");
        let mut additional_path_dirs = Vec::new();
        if venv_bin.exists() {
            additional_path_dirs.push(venv_bin.to_string_lossy().into_owned());
            env_vars.push((
                "VIRTUAL_ENV".to_string(),
                PathBuf::from(workspace_dir)
                    .join(".venv")
                    .to_string_lossy()
                    .into_owned(),
            ));
        }

        env_vars.push(("HOME".to_string(), workspace_dir.to_string()));

        let mut additional_read_paths: Vec<String> = Vec::new();
        if let Some(paths) = &manifest.read_paths {
            additional_read_paths.extend(paths.clone());
        }

        let config = SandboxConfig {
            workspace_dir: workspace_dir.to_string(),
            network_access: !allowed_destinations.is_empty(),
            allowed_network_destinations: allowed_destinations,
            additional_read_paths,
            additional_path_dirs,
            env_vars: env_vars.clone(),
            timeout_secs: 0, // no timeout for long-running processes
            ..Default::default()
        };

        let mut std_cmd = sandbox.sandboxed_command("bash", &["-c", command], &config)?;

        std_cmd.env_clear();

        const PASSTHROUGH_VARS: &[&str] =
            &["TERM", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "USER", "LOGNAME", "TMPDIR", "SHELL"];

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

        let log_dir = PathBuf::from(workspace_dir).join(".app_logs");
        let _ = std::fs::create_dir_all(&log_dir);

        std_cmd.stdout(std::process::Stdio::null());
        std_cmd.stderr(std::process::Stdio::null());

        let mut cmd = tokio::process::Command::from(std_cmd);
        let child = cmd
            .spawn()
            .map_err(|e| AppError::Tool(format!("Failed to spawn app process: {e}")))?;

        Ok(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_allocate_port_returns_sequential() {
        let manager = AppManager::new(
            PathBuf::from("/tmp/test_workspaces"),
            true,
            5000,
            5003,
        );

        let p1 = manager.allocate_port().await.unwrap();
        let p2 = manager.allocate_port().await.unwrap();
        let p3 = manager.allocate_port().await.unwrap();

        assert_eq!(p1, 5000);
        assert_eq!(p2, 5001);
        assert_eq!(p3, 5002);

        let p4 = manager.allocate_port().await;
        assert!(p4.is_err());
    }

    #[tokio::test]
    async fn test_allocate_port_reuses_after_free() {
        let manager = AppManager::new(
            PathBuf::from("/tmp/test_workspaces"),
            true,
            5000,
            5002,
        );

        let p1 = manager.allocate_port().await.unwrap();
        assert_eq!(p1, 5000);

        manager.allocated_ports.lock().await.remove(&5000);

        let p2 = manager.allocate_port().await.unwrap();
        assert_eq!(p2, 5000);
    }

    #[tokio::test]
    async fn test_record_and_get_access() {
        let manager = AppManager::new(
            PathBuf::from("/tmp/test_workspaces"),
            true,
            5000,
            5100,
        );

        assert!(manager.get_last_accessed("app1").await.is_none());

        manager.record_access("app1").await;
        assert!(manager.get_last_accessed("app1").await.is_some());
    }

    #[tokio::test]
    async fn test_flush_access_times() {
        let manager = AppManager::new(
            PathBuf::from("/tmp/test_workspaces"),
            true,
            5000,
            5100,
        );

        manager.record_access("app1").await;
        manager.record_access("app2").await;

        let flushed = manager.flush_access_times().await;
        assert_eq!(flushed.len(), 2);
        assert!(flushed.contains_key("app1"));

        assert!(manager.get_last_accessed("app1").await.is_none());
    }

    #[test]
    fn test_workspace_path_sanitizes() {
        let manager = AppManager::new(
            PathBuf::from("/tmp/test_workspaces"),
            true,
            5000,
            5100,
        );

        let path = manager.workspace_path("agent/with\\bad:chars");
        assert!(!path.contains('/') || path.starts_with("/tmp"));
        assert!(!path.contains('\\'));
    }
}
