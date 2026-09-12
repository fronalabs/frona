use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::core::error::AppError;
use crate::tool::sandbox::SandboxManager;

use super::models::{AppManifest, HealthCheck};

pub struct ProcessExit {
    pub status: Option<std::process::ExitStatus>,
    pub stderr_tail: String,
}

pub enum ProcessStatus {
    Alive,
    Dead(ProcessExit),
    NotManaged,
}

pub struct ManagedProcess {
    pub child: tokio::process::Child,
    pub port: u16,
    pub agent_id: String,
    pub user_id: String,
    pub manifest: AppManifest,
    pub credential_env_vars: Vec<(String, String)>,
    pub restart_count: u32,
    pub consecutive_failures: u32,
    pub log_path: Option<PathBuf>,
}

pub struct AppManager {
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    allocated_ports: Arc<Mutex<HashSet<u16>>>,
    port_range: (u16, u16),
    sandbox_manager: Arc<SandboxManager>,
    last_accessed: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    user_service: crate::auth::UserService,
    agent_service: crate::agent::service::AgentService,
    http: reqwest::Client,
}

impl AppManager {
    pub fn new(
        sandbox_manager: Arc<SandboxManager>,
        port_range_start: u16,
        port_range_end: u16,
        user_service: crate::auth::UserService,
        agent_service: crate::agent::service::AgentService,
        http: reqwest::Client,
    ) -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            allocated_ports: Arc::new(Mutex::new(HashSet::new())),
            port_range: (port_range_start, port_range_end),
            sandbox_manager,
            last_accessed: Arc::new(Mutex::new(HashMap::new())),
            user_service,
            agent_service,
            http,
        }
    }

    pub async fn start_app(
        &self,
        app_id: &str,
        agent_id: &str,
        user_id: &str,
        command: &str,
        manifest: &AppManifest,
        credential_env_vars: Vec<(String, String)>,
    ) -> Result<(u16, u32), AppError> {
        let port = self.allocate_port().await?;

        let (child, log_path) = self
            .spawn_process(
                agent_id,
                user_id,
                command,
                port,
                manifest,
                &credential_env_vars,
            )
            .await?;

        let pid = child.id().unwrap_or(0);

        let managed = ManagedProcess {
            child,
            port,
            agent_id: agent_id.to_string(),
            user_id: user_id.to_string(),
            manifest: manifest.clone(),
            credential_env_vars,
            restart_count: 0,
            consecutive_failures: 0,
            log_path: Some(log_path),
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

            kill_process_group(&mut proc.child).await;
            let _ = proc.child.wait().await;

            self.allocated_ports.lock().await.remove(&port);
        }
        Ok(())
    }

    pub async fn health_check(&self, port: u16, health_check: &HealthCheck) -> bool {
        let url = format!("http://127.0.0.1:{}{}", port, health_check.path);
        let timeout = std::time::Duration::from_secs(health_check.effective_timeout());
        self.http
            .get(&url)
            .timeout(timeout)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    pub async fn check_process(&self, app_id: &str) -> ProcessStatus {
        let mut processes = self.processes.lock().await;
        let Some(proc) = processes.get_mut(app_id) else {
            return ProcessStatus::NotManaged;
        };
        match proc.child.try_wait() {
            Ok(None) => ProcessStatus::Alive,
            Ok(Some(status)) => {
                let stderr_tail = proc
                    .log_path
                    .as_ref()
                    .map(|p| read_tail(p, 4096))
                    .unwrap_or_default();
                ProcessStatus::Dead(ProcessExit {
                    status: Some(status),
                    stderr_tail,
                })
            }
            Err(_) => ProcessStatus::Dead(ProcessExit {
                status: None,
                stderr_tail: String::new(),
            }),
        }
    }

    pub async fn restart_app(
        &self,
        app_id: &str,
        agent_id: &str,
    ) -> Result<Option<(u16, u32)>, AppError> {
        let (command, manifest, creds, user_id) = {
            let processes = self.processes.lock().await;
            match processes.get(app_id) {
                Some(proc) => (
                    proc.manifest.command.clone(),
                    proc.manifest.clone(),
                    proc.credential_env_vars.clone(),
                    proc.user_id.clone(),
                ),
                None => return Ok(None),
            }
        };

        self.stop_app(app_id).await?;

        if let Some(cmd) = command {
            let (port, pid) = self
                .start_app(app_id, agent_id, &user_id, &cmd, &manifest, creds)
                .await?;
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
                    "on_failure" => match proc.child.try_wait() {
                        Ok(Some(status)) => !status.success(),
                        _ => true,
                    },
                    _ => false,
                };

                if !should_restart {
                    return Ok(None);
                }

                proc.restart_count += 1;
                let restart_count = proc.restart_count;
                Some((
                    proc.agent_id.clone(),
                    proc.user_id.clone(),
                    proc.manifest.command.clone(),
                    proc.manifest.clone(),
                    proc.credential_env_vars.clone(),
                    restart_count,
                ))
            } else {
                None
            }
        };

        if let Some((agent_id, user_id, command, manifest, creds, restart_count)) = info {
            self.stop_app(app_id).await?;
            if let Some(cmd) = command {
                let (port, pid) = self
                    .start_app(app_id, &agent_id, &user_id, &cmd, &manifest, creds)
                    .await?;
                self.processes
                    .lock()
                    .await
                    .get_mut(app_id)
                    .expect("process just inserted by start_app")
                    .restart_count = restart_count;
                return Ok(Some((port, pid)));
            }
        }

        Ok(None)
    }

    pub async fn remove_process(&self, app_id: &str) {
        let mut processes = self.processes.lock().await;
        if let Some(proc) = processes.remove(app_id) {
            self.allocated_ports.lock().await.remove(&proc.port);
        }
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

    pub async fn restart_count_for(&self, app_id: &str) -> u32 {
        self.processes
            .lock()
            .await
            .get(app_id)
            .map(|p| p.restart_count)
            .unwrap_or(0)
    }

    pub async fn agent_id_for(&self, app_id: &str) -> Option<String> {
        self.processes
            .lock()
            .await
            .get(app_id)
            .map(|p| p.agent_id.clone())
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

    async fn spawn_process(
        &self,
        agent_id: &str,
        user_id: &str,
        command: &str,
        port: u16,
        manifest: &AppManifest,
        credential_env_vars: &[(String, String)],
    ) -> Result<(tokio::process::Child, PathBuf), AppError> {
        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("user {user_id}")))?;
        let agent = self.agent_service.get(user_id, agent_id).await?;

        let mut sandbox = self
            .sandbox_manager
            .for_app(&user, &agent, manifest, port, credential_env_vars.to_vec())
            .await?;

        let app_dir = sandbox.path().join("apps").join(manifest.handle.as_str());
        let app_log_dir = app_dir.join("logs");
        std::fs::create_dir_all(&app_log_dir)
            .map_err(|e| AppError::Tool(format!("Failed to create app directory: {e}")))?;

        let has_source_files = std::fs::read_dir(&app_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| e.file_name() != "logs")
            })
            .unwrap_or(false);

        if !has_source_files {
            return Err(AppError::Tool(format!(
                "No source files found in apps/{}/ — write your app code there before deploying",
                manifest.handle
            )));
        }

        let app_dir_str = app_dir.to_string_lossy().into_owned();

        let mut extra_path_dirs = Vec::new();
        let app_venv_bin = app_dir.join(".venv").join("bin");
        if app_venv_bin.exists() {
            extra_path_dirs.push(app_venv_bin.to_string_lossy().into_owned());
            sandbox = sandbox.with_extra_env_vars(vec![(
                "VIRTUAL_ENV".to_string(),
                app_dir.join(".venv").to_string_lossy().into_owned(),
            )]);
        }
        let app_bin = app_dir.join("bin");
        if app_bin.exists() {
            extra_path_dirs.push(app_bin.to_string_lossy().into_owned());
        }

        let log_path = app_log_dir.join("app.log");
        let log_file = std::fs::File::create(&log_path)
            .map_err(|e| AppError::Tool(format!("Failed to create app log: {e}")))?;
        let log_file_clone = log_file
            .try_clone()
            .map_err(|e| AppError::Tool(format!("Failed to clone log file handle: {e}")))?;

        let child = sandbox.spawn(
            "bash",
            &["-c", command],
            Some(&app_dir_str),
            extra_path_dirs,
            None,
            std::process::Stdio::from(log_file),
            std::process::Stdio::from(log_file_clone),
        )?;

        Ok((child, log_path))
    }
}

async fn kill_process_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Sandbox::spawn calls setsid(), making the tracked child the process-group
        // leader. Kill the group so shells and app servers do not survive a backend
        // restart and keep the allocated port bound.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
}

fn read_tail(path: &PathBuf, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(metadata) = file.metadata() else {
        return String::new();
    };
    let len = metadata.len();
    if len > max_bytes {
        let _ = file.seek(SeekFrom::End(-(max_bytes as i64)));
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::sandbox::SandboxFactory;

    async fn test_sandbox_manager(
        storage: crate::storage::StorageService,
    ) -> Arc<crate::tool::sandbox::SandboxManager> {
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let rm = Arc::new(
            crate::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                60.0, 60.0, 60.0, 60.0,
            ),
        );
        let factory = Arc::new(SandboxFactory::new(true, rm));

        let user_service = crate::auth::UserService::new(
            crate::db::repo::generic::SurrealRepo::new(db.clone()),
            &crate::core::config::CacheConfig::default(),
        );
        let tool_fixture = crate::app_state_fixture::build(&db).await;
        let tool_manager = tool_fixture.state.tool_manager.clone();
        let policy_repo: Arc<dyn crate::policy::repository::PolicyRepository> =
            Arc::new(crate::db::repo::generic::SurrealRepo::<
                crate::policy::models::Policy,
            >::new(db.clone()));
        let policy_service = crate::policy::service::PolicyService::new(
            policy_repo,
            crate::policy::schema::build_schema(),
            tool_manager,
            storage.clone(),
            user_service.clone(),
        );
        let skill_service = crate::agent::skill::service::SkillService::new(
            crate::agent::skill::registry::SkillRegistryClient::new(
                crate::build_http_client(),
                "/tmp/frona-test-skills-cache",
            ),
            crate::agent::skill::resolver::SkillResolver::new(
                "/tmp/frona-test-shared",
                storage.clone(),
                "/tmp/frona-test-skills",
            ),
            storage.clone(),
            "/tmp/frona-test-skills",
            &crate::core::config::CacheConfig::default(),
        );
        let keypair_repo: Arc<dyn crate::credential::keypair::repository::KeyPairRepository> =
            Arc::new(crate::db::repo::generic::SurrealRepo::<
                crate::credential::keypair::models::KeyPair,
            >::new(db.clone()));
        let keypair_service =
            crate::credential::keypair::service::KeyPairService::new("test-secret", keypair_repo);
        let token_repo: Arc<
            crate::db::repo::generic::SurrealRepo<crate::auth::token::models::ApiToken>,
        > = Arc::new(crate::db::repo::generic::SurrealRepo::new(db));
        let token_service = crate::auth::token::service::TokenService::new(
            token_repo,
            crate::auth::jwt::JwtService::new(),
            user_service,
            3600,
            86400,
        );

        Arc::new(crate::tool::sandbox::SandboxManager::new(
            factory,
            policy_service,
            skill_service,
            storage,
            token_service,
            keypair_service,
            "http://localhost".to_string(),
            300,
            "UTC".to_string(),
        ))
    }

    async fn test_manager(port_start: u16, port_end: u16) -> AppManager {
        let storage = crate::storage::StorageService::new(&crate::core::config::Config::default());
        AppManager::new(
            test_sandbox_manager(storage).await,
            port_start,
            port_end,
            test_user_service().await,
            test_agent_service().await,
            crate::build_http_client(),
        )
    }

    async fn test_agent_service() -> crate::agent::service::AgentService {
        use surrealdb::Surreal;
        use surrealdb::engine::local::Mem;
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let user_svc = crate::auth::UserService::new(
            crate::db::repo::generic::SurrealRepo::new(db.clone()),
            &crate::core::config::CacheConfig::default(),
        );
        let policy_repo: std::sync::Arc<dyn crate::policy::repository::PolicyRepository> =
            std::sync::Arc::new(crate::db::repo::generic::SurrealRepo::<
                crate::policy::models::Policy,
            >::new(db.clone()));
        let schema = crate::policy::schema::build_schema();
        let tool_fixture = crate::app_state_fixture::build(&db).await;
        let tool_manager = tool_fixture.state.tool_manager.clone();
        let storage = crate::storage::StorageService::new(&crate::core::config::Config::default());
        let policy_svc = crate::policy::service::PolicyService::new(
            policy_repo,
            schema,
            tool_manager,
            storage,
            user_svc.clone(),
        );
        let repo = crate::db::repo::agents::SurrealAgentRepo::new(db);
        use crate::core::repository::Repository;
        let now = chrono::Utc::now();
        let _ = repo
            .create(&crate::agent::models::Agent {
                id: "agent-1".into(),
                user_id: "user-1".into(),
                handle: crate::handle!("agent-1"),
                name: "Test Agent".into(),
                description: String::new(),
                model_group: "primary".into(),
                enabled: true,
                skills: None,
                sandbox_limits: None,
                max_concurrent_tasks: None,
                avatar: None,
                identity: std::collections::BTreeMap::new(),
                prompt: None,
                heartbeat_interval: None,
                next_heartbeat_at: None,
                heartbeat_chat_id: None,
                created_at: now,
                updated_at: now,
            })
            .await;
        crate::agent::service::AgentService::new(
            repo,
            &crate::core::config::CacheConfig::default(),
            std::sync::Arc::new(
                crate::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                    60.0, 60.0, 60.0, 60.0,
                ),
            ),
            policy_svc,
            user_svc,
        )
    }

    async fn test_user_service() -> crate::auth::UserService {
        use surrealdb::Surreal;
        use surrealdb::engine::local::Mem;
        let db = Surreal::new::<Mem>(()).await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let svc = crate::auth::UserService::new(
            crate::db::repo::generic::SurrealRepo::new(db),
            &crate::core::config::CacheConfig::default(),
        );
        let now = chrono::Utc::now();
        let user = crate::auth::User {
            id: "user-1".into(),
            handle: crate::handle!("user-1"),
            email: "test@example.com".into(),
            name: "Test".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: now,
            updated_at: now,
        };
        svc.create(&user).await.expect("seed test user-1");
        svc
    }

    #[tokio::test]
    async fn test_allocate_port_returns_sequential() {
        let manager = test_manager(5000, 5003).await;

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
        let manager = test_manager(5000, 5002).await;

        let p1 = manager.allocate_port().await.unwrap();
        assert_eq!(p1, 5000);

        manager.allocated_ports.lock().await.remove(&5000);

        let p2 = manager.allocate_port().await.unwrap();
        assert_eq!(p2, 5000);
    }

    #[tokio::test]
    async fn test_record_and_get_access() {
        let manager = test_manager(5000, 5100).await;

        assert!(manager.get_last_accessed("app1").await.is_none());

        manager.record_access("app1").await;
        assert!(manager.get_last_accessed("app1").await.is_some());
    }

    #[tokio::test]
    async fn test_flush_access_times() {
        let manager = test_manager(5000, 5100).await;

        manager.record_access("app1").await;
        manager.record_access("app2").await;

        let flushed = manager.flush_access_times().await;
        assert_eq!(flushed.len(), 2);
        assert!(flushed.contains_key("app1"));

        assert!(manager.get_last_accessed("app1").await.is_none());
    }

    #[tokio::test]
    async fn test_restart_count_preserved_after_crash_restart() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let workspaces = tmp.path();

        let app_dir = workspaces
            .join("users")
            .join("user-1")
            .join("agents")
            .join("agent-1")
            .join("apps")
            .join("test-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("run.sh"), "#!/bin/sh\ntrue").unwrap();

        let mut test_cfg = crate::core::config::Config::default();
        test_cfg.storage.data_dir = workspaces.to_string_lossy().into_owned();
        let storage = crate::storage::StorageService::new(&test_cfg);
        let manager = AppManager::new(
            test_sandbox_manager(storage).await,
            6000,
            6010,
            test_user_service().await,
            test_agent_service().await,
            crate::build_http_client(),
        );

        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("failed to spawn dummy process");

        let manifest = crate::app::models::AppManifest {
            handle: crate::handle!("test-app"),
            name: "Test".to_string(),
            description: None,
            icon: None,
            kind: None,
            command: Some("true".to_string()),
            restart_policy: Some("always".to_string()),
            health_check: None,
            resources: None,
            static_dir: None,
            expose: None,
            sandbox_policy: None,
            credentials: None,
            hibernate: None,
        };

        let port = manager.allocate_port().await.unwrap();
        manager.processes.lock().await.insert(
            "test-app".to_string(),
            ManagedProcess {
                child,
                port,
                agent_id: "agent-1".to_string(),
                user_id: "user-1".to_string(),
                manifest,
                credential_env_vars: Vec::new(),
                restart_count: 0,
                consecutive_failures: 0,
                log_path: None,
            },
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let result = manager.try_restart_crashed("test-app", 3).await.unwrap();
        assert!(result.is_some(), "restart should succeed");

        let processes = manager.processes.lock().await;
        let proc = processes.get("test-app").expect("process should exist");
        assert_eq!(
            proc.restart_count, 1,
            "restart_count should be preserved as 1"
        );
    }

    #[test]
    fn test_app_dir_structure() {
        let workspace = PathBuf::from("/tmp/workspaces/agent_123");
        let manifest_id = "my-dashboard";

        let app_dir = workspace.join("apps").join(manifest_id);
        let log_dir = app_dir.join("logs");
        let log_path = log_dir.join("app.log");

        assert_eq!(
            app_dir,
            PathBuf::from("/tmp/workspaces/agent_123/apps/my-dashboard")
        );
        assert_eq!(
            log_dir,
            PathBuf::from("/tmp/workspaces/agent_123/apps/my-dashboard/logs")
        );
        assert_eq!(
            log_path,
            PathBuf::from("/tmp/workspaces/agent_123/apps/my-dashboard/logs/app.log")
        );
    }
}
