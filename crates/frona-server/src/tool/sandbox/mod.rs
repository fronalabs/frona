pub mod driver;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::error::AppError;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::driver::resource_monitor::SystemResourceManager;
use self::driver::{SandboxConfig, SandboxOutput, create_driver, execute_sandboxed};
use crate::auth::ephemeral_token::EphemeralTokenGuard;
use crate::core::Principal;

/// Pre-resolved at factory construction so per-call `allows()` matches
/// canonical forms without re-canonicalising on every tool call.
pub struct BaseFilesystemPolicy {
    pub system_read_dirs: Vec<PathBuf>,
    pub proc_read_paths: Vec<PathBuf>,
    pub etc_read_allowlist: Vec<PathBuf>,
    pub read_write_dirs: Vec<PathBuf>,
    pub read_write_devices: Vec<PathBuf>,
}

impl BaseFilesystemPolicy {
    fn from_driver_constants() -> Self {
        let canon = |entries: &[&str]| -> Vec<PathBuf> {
            entries
                .iter()
                .map(|p| canonicalize_with_unresolved_tail(Path::new(p)))
                .collect()
        };
        Self {
            system_read_dirs: canon(driver::linux::SYSTEM_READ_DIRS),
            proc_read_paths: canon(driver::linux::PROC_READ_PATHS),
            etc_read_allowlist: canon(driver::ETC_READ_ALLOWLIST),
            read_write_dirs: canon(driver::linux::READ_WRITE_DIRS),
            read_write_devices: canon(driver::linux::READ_WRITE_DEVICES),
        }
    }
}

/// Thin factory that owns the platform-specific sandbox driver and the
/// process resource manager. Hands out `Sandbox` instances configured with
/// the driver - no service knowledge, no orchestration. All production
/// callers go through [`SandboxManager`]; the factory itself is only
/// reached directly for integration tests and for the install-phase
/// permissive bypass in `McpManager::build_install_sandbox` (which skips
/// Cedar deliberately).
pub struct SandboxFactory {
    driver: Arc<dyn driver::SandboxDriver>,
    shared_read_paths: Vec<String>,
    resource_manager: Arc<SystemResourceManager>,
    default_timeout_secs: u64,
    base_filesystem_policy: Arc<BaseFilesystemPolicy>,
}

impl SandboxFactory {
    pub fn new(sandbox_disabled: bool, resource_manager: Arc<SystemResourceManager>) -> Self {
        Self {
            driver: Arc::from(create_driver(sandbox_disabled)),
            shared_read_paths: Vec::new(),
            resource_manager,
            default_timeout_secs: 0,
            base_filesystem_policy: Arc::new(BaseFilesystemPolicy::from_driver_constants()),
        }
    }

    pub fn driver_id(&self) -> &'static str {
        self.driver.driver_id()
    }

    pub fn with_default_timeout(mut self, secs: u64) -> Self {
        self.default_timeout_secs = secs;
        self
    }

    pub fn default_timeout_secs(&self) -> u64 {
        self.default_timeout_secs
    }

    pub fn resource_manager(&self) -> &Arc<SystemResourceManager> {
        &self.resource_manager
    }

    pub fn with_shared_read_paths(mut self, paths: Vec<String>) -> Self {
        self.shared_read_paths = paths;
        self
    }

    pub fn get_sandbox(
        &self,
        workspace: PathBuf,
        agent_id: &str,
        network_access: bool,
        allowed_network_destinations: Vec<String>,
    ) -> Sandbox {
        Sandbox {
            path: workspace,
            driver: Arc::clone(&self.driver),
            network_access,
            allowed_network_destinations,
            allowed_bind_ports: Vec::new(),
            extra_env_vars: Vec::new(),
            shared_read_paths: self.shared_read_paths.clone(),
            shared_read_files: Vec::new(),
            shared_write_paths: Vec::new(),
            denied_paths: Vec::new(),
            blocked_networks: Vec::new(),
            agent_id: agent_id.to_string(),
            resource_manager: Arc::clone(&self.resource_manager),
            init_venv: true,
            init_node: true,
            token_guard: None,
            base_filesystem_policy: Arc::clone(&self.base_filesystem_policy),
        }
    }
}

/// Single entry point for building a fully-configured [`Sandbox`]. Owns
/// the Cedar policy + workspace + token + env machinery and exposes one
/// constructor per principal kind:
/// - [`SandboxManager::for_tool`] - agent inference tools (`CliTool`,
///   the typed file tools). Adds skill paths and an ephemeral token.
/// - [`SandboxManager::for_command`] - command tools, including current vault bindings.
/// - [`SandboxManager::for_app`] - App processes under an agent workspace.
/// - [`SandboxManager::for_mcp`] - MCP servers in their own workspace.
///
/// Wraps a [`SandboxFactory`] internally; exposes it via
/// [`SandboxManager::factory`] for the two callers that need raw factory
/// surface (`CliTool` for `default_timeout_secs` / `resource_manager`,
/// `McpManager::build_install_sandbox` for the permissive install path).
pub struct SandboxManager {
    factory: Arc<SandboxFactory>,
    policy_service: crate::policy::service::PolicyService,
    skill_service: crate::agent::skill::service::SkillService,
    storage_service: crate::storage::service::StorageService,
    token_service: crate::auth::token::service::TokenService,
    keypair_service: crate::credential::keypair::service::KeyPairService,
    api_base_url: String,
    ephemeral_token_expiry_secs: u64,
    server_timezone: String,
    vault_service: crate::credential::vault::service::VaultService,
}

#[allow(clippy::too_many_arguments)]
impl SandboxManager {
    pub fn new(
        factory: Arc<SandboxFactory>,
        policy_service: crate::policy::service::PolicyService,
        skill_service: crate::agent::skill::service::SkillService,
        storage_service: crate::storage::service::StorageService,
        token_service: crate::auth::token::service::TokenService,
        keypair_service: crate::credential::keypair::service::KeyPairService,
        vault_service: crate::credential::vault::service::VaultService,
        api_base_url: String,
        ephemeral_token_expiry_secs: u64,
        server_timezone: String,
    ) -> Self {
        Self {
            factory,
            policy_service,
            skill_service,
            storage_service,
            token_service,
            keypair_service,
            api_base_url,
            ephemeral_token_expiry_secs,
            server_timezone,
            vault_service,
        }
    }

    /// Build a command sandbox with freshly authorized credentials.
    pub async fn for_command(
        &self,
        ctx: &crate::inference::request::InferenceContext,
    ) -> Result<Sandbox, AppError> {
        let mut sandbox = self.for_tool(ctx).await?;
        let env = self
            .vault_service
            .resolve_env(
                &ctx.user.id,
                &Principal::agent(&ctx.agent.id),
                ctx.chat.as_ref().map(|chat| chat.id.as_str()),
            )
            .await?;
        // Reserved Frona variables stay last, as in the other launch paths.
        sandbox.extra_env_vars.splice(0..0, env);
        Ok(sandbox)
    }

    /// Underlying factory - useful for callers that need things like
    /// `default_timeout_secs` or `resource_manager` without going through
    /// `for_tool`.
    pub fn factory(&self) -> &SandboxFactory {
        &self.factory
    }

    /// Build a fully-configured Sandbox for an agent: Cedar policy + skill
    /// grants + workspace + ctx.file_paths + ephemeral token guard +
    /// API env vars. File tools use this for path checks without resolving
    /// credentials. Command tools use `for_command` before execution.
    pub async fn for_tool(
        &self,
        ctx: &crate::inference::request::InferenceContext,
    ) -> Result<Sandbox, AppError> {
        let agent_id = &ctx.agent.id;

        let policy = self
            .policy_service
            .evaluate_sandbox_policy(
                crate::policy::service::SandboxPrincipalRef::agent(
                    &ctx.user.id,
                    &ctx.user.handle,
                    &ctx.agent.handle,
                ),
                true,
            )
            .await?;

        let skill_read_paths: Vec<String> = self
            .skill_service
            .list(
                &ctx.user.handle,
                &ctx.agent.handle,
                ctx.agent.skills.as_deref(),
            )
            .await
            .into_iter()
            .map(|s| s.path)
            .collect();

        let workspace = self
            .storage_service
            .agent_workspace_path(&ctx.user.handle, &ctx.agent.handle);

        // Ambient grant: if the agent is authorized for the `memory` tool group,
        // expose the user's memory dir read-only (the PKM page tree / Obsidian
        // vault). Read-only - pages are derived, never agent-written.
        let mut memory_read: Vec<String> = Vec::new();
        if self
            .policy_service
            .is_tool_group_permitted(&ctx.user.id, &ctx.user.handle, &ctx.agent.handle, "memory")
            .await
            .unwrap_or(false)
        {
            let memory_path = self.storage_service.user_pkm_path(&ctx.user.handle);
            memory_read.push(
                canonicalize_with_unresolved_tail(&memory_path)
                    .to_string_lossy()
                    .into_owned(),
            );
        }

        let mut sandbox = self
            .factory
            .get_sandbox(
                workspace,
                agent_id,
                policy.network_access,
                policy.network_destinations.clone(),
            )
            .with_read_paths(skill_read_paths)
            .with_read_paths(memory_read)
            .with_read_paths(policy.read_paths.clone())
            .with_write_paths(policy.write_paths.clone())
            .with_denied_paths(policy.denied_paths.clone())
            .with_blocked_networks(policy.blocked_networks.clone())
            .with_bind_ports(policy.bind_ports.clone());

        if !ctx.file_paths.is_empty() {
            sandbox = sandbox.with_write_paths(ctx.file_paths.clone());
        }

        let tokens_dir = self.storage_service.user_tokens_path(&ctx.user.handle);
        let token_guard = EphemeralTokenGuard::issue(
            &self.token_service,
            &self.keypair_service,
            &ctx.user,
            Principal::agent(agent_id),
            self.ephemeral_token_expiry_secs,
            &tokens_dir,
        )
        .await?;

        sandbox = sandbox.with_read_files(vec![token_guard.path().to_string_lossy().into_owned()]);

        {
            let extra_vars = vec![
                (
                    "TZ".to_string(),
                    ctx.user.resolved_timezone(&self.server_timezone),
                ),
                (
                    "FRONA_TOKEN_FILE".to_string(),
                    token_guard.path().to_string_lossy().into_owned(),
                ),
                ("FRONA_API_URL".to_string(), self.api_base_url.clone()),
            ];
            sandbox = sandbox.with_extra_env_vars(extra_vars);
        }

        sandbox.token_guard = Some(token_guard);
        Ok(sandbox)
    }

    /// Build a Sandbox for an App process running under an agent's workspace.
    /// Evaluates Cedar for `SandboxPrincipalRef::app`, applies the resulting
    /// policy, opens `127.0.0.1:{port}` for the reverse proxy regardless of
    /// policy, and prepends `PORT={port}` to the env.
    pub async fn for_app(
        &self,
        user: &crate::auth::User,
        agent: &crate::agent::models::Agent,
        manifest: &crate::app::models::AppManifest,
        port: u16,
        extra_env: Vec<(String, String)>,
    ) -> Result<Sandbox, AppError> {
        let policy = self
            .policy_service
            .evaluate_sandbox_policy(
                crate::policy::service::SandboxPrincipalRef::app(
                    &user.id,
                    &user.handle,
                    &manifest.handle,
                ),
                true,
            )
            .await?;

        let workspace = self
            .storage_service
            .agent_workspace_path(&user.handle, &agent.handle);

        let mut network_dests = vec![format!("127.0.0.1:{port}")];
        network_dests.extend(policy.network_destinations.iter().cloned());

        let mut env = vec![("PORT".to_string(), port.to_string())];
        env.extend(extra_env);

        let sandbox = self
            .factory
            .get_sandbox(workspace, &agent.id, policy.network_access, network_dests)
            .with_bind_ports(vec![port])
            .with_extra_env_vars(env)
            .with_read_paths(policy.read_paths.clone())
            .with_write_paths(policy.write_paths.clone())
            .with_denied_paths(policy.denied_paths.clone())
            .with_blocked_networks(policy.blocked_networks.clone());

        Ok(sandbox)
    }

    /// Build a Sandbox for an MCP server in its own workspace. Evaluates
    /// Cedar for `SandboxPrincipalRef::mcp`, applies the policy, disables
    /// venv/Node setup (MCP runtimes bring their own toolchains).
    pub async fn for_mcp(
        &self,
        user: &crate::auth::User,
        server: &crate::tool::mcp::McpServer,
        extra_env: Vec<(String, String)>,
        token_path: Option<&Path>,
    ) -> Result<Sandbox, AppError> {
        let policy = self
            .policy_service
            .evaluate_sandbox_policy(
                crate::policy::service::SandboxPrincipalRef::mcp(
                    &server.user_id,
                    &user.handle,
                    &server.handle,
                ),
                true,
            )
            .await?;

        let sandbox_id = format!("mcp-{}", server.id);
        let workspace = self
            .storage_service
            .mcp_workspace_path(&user.handle, &server.handle);
        let mut sandbox = self
            .factory
            .get_sandbox(
                workspace,
                &sandbox_id,
                policy.network_access,
                policy.network_destinations.clone(),
            )
            .without_venv()
            .without_node()
            .with_read_paths(policy.read_paths.clone())
            .with_write_paths(policy.write_paths.clone())
            .with_denied_paths(policy.denied_paths.clone())
            .with_blocked_networks(policy.blocked_networks.clone())
            .with_bind_ports(policy.bind_ports.clone())
            .with_extra_env_vars(extra_env);
        if let Some(path) = token_path {
            sandbox = sandbox.with_read_files(vec![path.to_string_lossy().into_owned()]);
        }
        Ok(sandbox)
    }
}

pub struct Sandbox {
    path: PathBuf,
    driver: Arc<dyn driver::SandboxDriver>,
    network_access: bool,
    allowed_network_destinations: Vec<String>,
    allowed_bind_ports: Vec<u16>,
    extra_env_vars: Vec<(String, String)>,
    shared_read_paths: Vec<String>,
    shared_read_files: Vec<String>,
    shared_write_paths: Vec<String>,
    denied_paths: Vec<String>,
    blocked_networks: Vec<String>,
    agent_id: String,
    resource_manager: Arc<SystemResourceManager>,
    init_venv: bool,
    init_node: bool,
    /// Holds the per-invocation ephemeral token so it lives as long as the
    /// Sandbox. `Drop` cleans up the token file. Only set by `for_tool`.
    token_guard: Option<EphemeralTokenGuard>,
    /// Canonical forms of the driver-hardcoded allow-list paths. Computed
    /// once at `SandboxFactory` construction and shared via `Arc` across
    /// every `Sandbox` it produces. Used by `allows()` so the per-call
    /// check iterates over canonical forms without re-canonicalising on
    /// every tool invocation.
    base_filesystem_policy: Arc<BaseFilesystemPolicy>,
}

impl Sandbox {
    pub fn with_extra_env_vars(mut self, vars: Vec<(String, String)>) -> Self {
        self.extra_env_vars = vars;
        self
    }

    pub fn with_read_paths(mut self, paths: Vec<String>) -> Self {
        self.shared_read_paths.extend(paths);
        self
    }

    pub fn with_read_files(mut self, files: Vec<String>) -> Self {
        self.shared_read_files.extend(files);
        self
    }

    pub fn with_write_paths(mut self, paths: Vec<String>) -> Self {
        self.shared_write_paths.extend(paths);
        self
    }

    pub fn with_bind_ports(mut self, ports: Vec<u16>) -> Self {
        self.allowed_bind_ports = ports;
        self
    }

    pub fn with_denied_paths(mut self, paths: Vec<String>) -> Self {
        self.denied_paths.extend(paths);
        self
    }

    pub fn with_blocked_networks(mut self, networks: Vec<String>) -> Self {
        self.blocked_networks.extend(networks);
        self
    }

    pub fn without_venv(mut self) -> Self {
        self.init_venv = false;
        self
    }

    pub fn without_node(mut self) -> Self {
        self.init_node = false;
        self
    }
}

impl Sandbox {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mirrors the same policy the sandbox driver applies to subprocesses,
    /// so in-process file tools enforce the same allow-list as a spawned
    /// sandbox without paying for a subprocess.
    pub fn is_readable(&self, path: &Path) -> bool {
        self.allows(path, AccessKind::Read)
    }

    /// Would the sandbox grant write access to this path?
    pub fn is_writable(&self, path: &Path) -> bool {
        self.allows(path, AccessKind::Write)
    }

    fn allows(&self, path: &Path, kind: AccessKind) -> bool {
        let canonical = canonicalize_with_unresolved_tail(path);

        // Denied paths shadow everything.
        for d in &self.denied_paths {
            let denied = canonicalize_with_unresolved_tail(Path::new(d));
            if canonical.starts_with(&denied) {
                return false;
            }
        }

        let workspace = self.workspace_dir();

        // Driver-hardcoded readable directories - pre-canonicalised at
        // factory construction.
        for d in self
            .base_filesystem_policy
            .system_read_dirs
            .iter()
            .chain(self.base_filesystem_policy.proc_read_paths.iter())
            .chain(self.base_filesystem_policy.etc_read_allowlist.iter())
        {
            if canonical.starts_with(d) {
                return kind == AccessKind::Read
                    || self.workspace_or_grant_writable(&canonical, &workspace);
            }
        }

        // Driver-hardcoded R+W dirs (e.g. /tmp).
        for d in &self.base_filesystem_policy.read_write_dirs {
            if canonical.starts_with(d) {
                return true;
            }
        }

        // Driver-hardcoded R+W devices.
        for d in &self.base_filesystem_policy.read_write_devices {
            if canonical == *d {
                return true;
            }
        }

        // Workspace (R+W, no Cedar required).
        if canonical.starts_with(&workspace) {
            return true;
        }

        // Workspace ancestors (read-only, for realpath traversal).
        if kind == AccessKind::Read {
            let mut ancestor = workspace.parent();
            while let Some(parent) = ancestor {
                if parent == Path::new("/") {
                    break;
                }
                if canonical == parent {
                    return true;
                }
                ancestor = parent.parent();
            }
        }

        // Cedar-derived + skill-derived read grants.
        if kind == AccessKind::Read {
            for p in &self.shared_read_paths {
                let grant = canonicalize_with_unresolved_tail(Path::new(p));
                if canonical.starts_with(&grant) {
                    return true;
                }
            }
            for f in &self.shared_read_files {
                let grant = canonicalize_with_unresolved_tail(Path::new(f));
                if canonical == grant {
                    return true;
                }
            }
        }

        // Cedar-derived write grants are R+W under the same prefix.
        for p in &self.shared_write_paths {
            let grant = canonicalize_with_unresolved_tail(Path::new(p));
            if canonical.starts_with(&grant) {
                return true;
            }
        }

        false
    }

    fn workspace_dir(&self) -> PathBuf {
        canonicalize_with_unresolved_tail(&self.path)
    }

    fn workspace_or_grant_writable(&self, path: &Path, workspace: &Path) -> bool {
        if path.starts_with(workspace) {
            return true;
        }
        for p in &self.shared_write_paths {
            let grant = canonicalize_with_unresolved_tail(Path::new(p));
            if path.starts_with(&grant) {
                return true;
            }
        }
        false
    }

    pub fn venv_path(&self) -> PathBuf {
        self.path.join(".venv")
    }

    pub fn setup(&self) -> Result<(), AppError> {
        self.ensure_dir()?;
        if self.init_venv {
            self.setup_venv();
        }
        if self.init_node {
            self.setup_node_env();
        }
        Ok(())
    }

    pub fn ensure_dir(&self) -> Result<(), AppError> {
        if !self.path.exists() {
            std::fs::create_dir_all(&self.path).map_err(|e| {
                AppError::Tool(format!(
                    "Failed to create sandbox dir {}: {e}",
                    self.path.display()
                ))
            })?;
        }
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

    fn setup_node_env(&self) {
        let node_prefix = self.path.join(".node");
        if node_prefix.exists() {
            return;
        }

        if let Err(e) = std::fs::create_dir_all(&node_prefix) {
            tracing::warn!(error = %e, "Failed to create .node prefix directory");
            return;
        }

        if !self.path.join("package.json").exists() {
            let result = std::process::Command::new("npm")
                .args(["init", "-y"])
                .current_dir(&self.path)
                .output();

            match result {
                Ok(output) if output.status.success() => {
                    tracing::info!(path = %self.path.display(), "Initialized npm workspace");
                }
                Ok(output) => {
                    tracing::warn!(
                        stderr = String::from_utf8_lossy(&output.stderr).as_ref(),
                        "Failed to run npm init"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "npm not available, skipping Node.js env setup");
                }
            }
        }
    }

    fn base_config(&self) -> Result<SandboxConfig, AppError> {
        self.setup()?;

        let canonical_path =
            std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());

        let mut additional_path_dirs = Vec::new();
        let mut env_vars = Vec::new();

        let venv_bin = canonical_path.join(".venv").join("bin");
        if venv_bin.exists() {
            additional_path_dirs.push(venv_bin.to_string_lossy().into_owned());
            env_vars.push((
                "VIRTUAL_ENV".to_string(),
                canonical_path.join(".venv").to_string_lossy().into_owned(),
            ));
        }

        let (node_path_dirs, node_env) = node_env_vars(&canonical_path);
        additional_path_dirs.extend(node_path_dirs);
        env_vars.extend(node_env);

        env_vars.push((
            "HOME".to_string(),
            canonical_path.to_string_lossy().into_owned(),
        ));
        env_vars.push((
            "XDG_CONFIG_HOME".to_string(),
            canonical_path
                .join(".config")
                .to_string_lossy()
                .into_owned(),
        ));
        env_vars.push((
            "XDG_CACHE_HOME".to_string(),
            canonical_path.join(".cache").to_string_lossy().into_owned(),
        ));

        env_vars.extend(self.extra_env_vars.clone());

        let config = SandboxConfig {
            workspace_dir: canonical_path.to_string_lossy().into_owned(),
            network_access: self.network_access,
            allowed_network_destinations: self.allowed_network_destinations.clone(),
            allowed_bind_ports: self.allowed_bind_ports.clone(),
            additional_read_paths: self.shared_read_paths.clone(),
            additional_read_files: self.shared_read_files.clone(),
            additional_write_paths: self.shared_write_paths.clone(),
            denied_paths: self.denied_paths.clone(),
            blocked_networks: self.blocked_networks.clone(),
            additional_path_dirs,
            env_vars,
            ..Default::default()
        };

        Ok(config)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        program: &str,
        args: &[&str],
        working_dir: Option<&str>,
        extra_path_dirs: Vec<String>,
        stdin: Option<std::process::Stdio>,
        stdout: std::process::Stdio,
        stderr: std::process::Stdio,
    ) -> Result<tokio::process::Child, AppError> {
        let mut config = self.base_config()?;

        config.additional_path_dirs.extend(extra_path_dirs);

        if let Some(wd) = working_dir {
            let canonical_wd = std::fs::canonicalize(wd)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| wd.to_string());
            config.working_dir = Some(canonical_wd);
        }

        let mut cmd = self.driver.sandboxed_command(program, args, &config)?;

        cmd.env_clear();

        const PASSTHROUGH_VARS: &[&str] = &[
            "TERM", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "USER", "LOGNAME", "TMPDIR", "SHELL",
        ];

        for key in PASSTHROUGH_VARS {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }

        {
            let existing = std::env::var("PATH").unwrap_or_default();
            if config.additional_path_dirs.is_empty() {
                cmd.env("PATH", existing);
            } else {
                let extra = config.additional_path_dirs.join(":");
                cmd.env("PATH", format!("{extra}:{existing}"));
            }
        }

        for (key, value) in &config.env_vars {
            cmd.env(key, value);
        }

        if let Some(stdin) = stdin {
            cmd.stdin(stdin);
        }
        cmd.stdout(stdout);
        cmd.stderr(stderr);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let child = tokio::process::Command::from(cmd)
            .spawn()
            .map_err(|e| AppError::Tool(format!("Failed to spawn process: {e}")))?;

        if let Some(pid) = child.id() {
            self.resource_manager.register(pid, &self.agent_id);
        }

        Ok(child)
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
        let mut config = self.base_config()?;
        config.timeout_secs = timeout_secs;

        execute_sandboxed(
            &*self.driver,
            program,
            args,
            &config,
            on_stdout,
            stdin_rx,
            cancel_token,
            Some(&self.resource_manager),
            Some(&self.agent_id),
        )
        .await
    }
}

#[derive(PartialEq, Eq)]
enum AccessKind {
    Read,
    Write,
}

/// Canonicalise `path`. If the full path doesn't exist (e.g. we're about to
/// create a new file), walk up to the longest-existing ancestor, canonicalise
/// that, then re-attach the non-existent tail. Result matches the symlink-
/// resolved form the OS would observe when the file is later created.
///
/// Used everywhere the sandbox compares path prefixes so the in-process
/// `is_readable` / `is_writable` checks stay consistent with what `syd` sees
/// at the OS layer (workspace path → canonical via symlinks; new-file target
/// → canonical-prefix + literal tail).
pub fn canonicalize_with_unresolved_tail(path: &Path) -> PathBuf {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path);
        &absolute
    };
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&cursor) {
            let mut result = canonical;
            for component in tail.into_iter().rev() {
                result.push(component);
            }
            return result;
        }
        let Some(last) = cursor.file_name().map(|n| n.to_owned()) else {
            return path.to_path_buf();
        };
        tail.push(last);
        if !cursor.pop() {
            return path.to_path_buf();
        }
    }
}

pub fn node_env_vars(workspace: &Path) -> (Vec<String>, Vec<(String, String)>) {
    let node_prefix = workspace.join(".node");
    if !node_prefix.exists() {
        return (Vec::new(), Vec::new());
    }
    let mut path_dirs = Vec::new();
    let bin = node_prefix.join("bin");
    if bin.exists() {
        path_dirs.push(bin.to_string_lossy().into_owned());
    }
    let env = vec![
        (
            "NPM_CONFIG_PREFIX".into(),
            node_prefix.to_string_lossy().into_owned(),
        ),
        (
            "NPM_CONFIG_CACHE".into(),
            workspace.join(".npm-cache").to_string_lossy().into_owned(),
        ),
        (
            "NODE_PATH".into(),
            workspace
                .join("node_modules")
                .to_string_lossy()
                .into_owned(),
        ),
    ];
    (path_dirs, env)
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
            driver: Arc::from(create_driver(false)),
            network_access: false,
            allowed_network_destinations: Vec::new(),
            allowed_bind_ports: Vec::new(),
            extra_env_vars: Vec::new(),
            shared_read_paths: Vec::new(),
            shared_read_files: Vec::new(),
            shared_write_paths: Vec::new(),
            denied_paths: Vec::new(),
            blocked_networks: Vec::new(),
            agent_id: "test".to_string(),
            resource_manager: Arc::new(SystemResourceManager::new(80.0, 80.0, 90.0, 90.0)),
            init_venv: true,
            init_node: true,
            token_guard: None,
            base_filesystem_policy: Arc::new(BaseFilesystemPolicy::from_driver_constants()),
        }
    }

    fn without_base_filesystem_grants(mut sandbox: Sandbox) -> Sandbox {
        sandbox.base_filesystem_policy = Arc::new(BaseFilesystemPolicy {
            system_read_dirs: Vec::new(),
            proc_read_paths: Vec::new(),
            etc_read_allowlist: Vec::new(),
            read_write_dirs: Vec::new(),
            read_write_devices: Vec::new(),
        });
        sandbox
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
            driver: Arc::from(create_driver(false)),
            network_access: false,
            allowed_network_destinations: Vec::new(),
            allowed_bind_ports: Vec::new(),
            extra_env_vars: Vec::new(),
            shared_read_paths: Vec::new(),
            shared_read_files: Vec::new(),
            shared_write_paths: Vec::new(),
            denied_paths: Vec::new(),
            blocked_networks: Vec::new(),
            agent_id: "test".to_string(),
            resource_manager: Arc::new(SystemResourceManager::new(80.0, 80.0, 90.0, 90.0)),
            init_venv: true,
            init_node: true,
            token_guard: None,
            base_filesystem_policy: Arc::new(BaseFilesystemPolicy::from_driver_constants()),
        };
        ws.setup().unwrap();

        assert!(
            rel_path.join(".venv").exists(),
            "venv should exist at the workspace root"
        );
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
                &[
                    "-c",
                    "echo $HOME; echo $XDG_CONFIG_HOME; echo $XDG_CACHE_HOME",
                ],
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
        assert_eq!(
            lines[1],
            canonical.join(".config").to_string_lossy().as_ref()
        );
        assert_eq!(
            lines[2],
            canonical.join(".cache").to_string_lossy().as_ref()
        );

        let _ = std::fs::remove_dir_all(&ws.path);
    }

    #[tokio::test]
    async fn test_execute_does_not_leak_parent_env() {
        unsafe { std::env::set_var("FRONA_TEST_SECRET", "leaked") };
        let ws = temp_sandbox(&format!("env_leak_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&ws.path);

        let result = ws
            .execute(
                "bash",
                &["-c", "echo $FRONA_TEST_SECRET"],
                10,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            result.stdout.trim(),
            "",
            "parent env vars must not leak into sandbox"
        );

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

    #[test]
    fn canonicalize_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("foo.txt");
        std::fs::write(&file, "hi").unwrap();
        let result = canonicalize_with_unresolved_tail(&file);
        // canonicalize should resolve any platform-level symlinks (e.g.,
        // /tmp → /private/tmp on macOS).
        assert_eq!(result, std::fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn canonicalize_nonexistent_file_existing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("does_not_exist.txt");
        let result = canonicalize_with_unresolved_tail(&target);
        let expected_parent = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(result, expected_parent.join("does_not_exist.txt"));
    }

    #[test]
    fn canonicalize_deeply_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a").join("b").join("c").join("file.txt");
        let result = canonicalize_with_unresolved_tail(&target);
        let expected_parent = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(result, expected_parent.join("a/b/c/file.txt"));
    }

    #[test]
    fn canonicalize_relative_nonexistent_path_is_absolute() {
        let relative = PathBuf::from("target")
            .join("test_data")
            .join("users/mina/pkm");
        let result = canonicalize_with_unresolved_tail(&relative);

        assert!(result.is_absolute());
        assert!(result.ends_with("target/test_data/users/mina/pkm"));
    }

    #[test]
    fn is_writable_nonexistent_target_under_symlinked_workspace() {
        // Simulate the docker case: workspace path goes through a symlink,
        // and the target file we want to write doesn't exist yet.
        let tmp = tempfile::tempdir().unwrap();
        let real_workspace = tmp.path().join("real_data").join("workspace");
        std::fs::create_dir_all(&real_workspace).unwrap();

        // Create a symlink "data" → "real_data" and address workspace via it.
        let symlinked_data = tmp.path().join("data");
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("real_data"), &symlinked_data).unwrap();
        #[cfg(not(unix))]
        return; // Symlink semantics differ on Windows; skip.

        let workspace_via_symlink = symlinked_data.join("workspace");
        let target = workspace_via_symlink.join("pwgen.py"); // doesn't exist

        // Exclude the driver's global /tmp grant so this specifically tests
        // workspace path matching.
        let mut ws = without_base_filesystem_grants(temp_sandbox(&format!(
            "symlink_{}",
            uuid::Uuid::new_v4()
        )));
        ws.path = workspace_via_symlink;

        // Pre-fix bug: is_writable returned false because target was literal
        // (file doesn't exist) but workspace was canonical (/var/.../workspace
        // through the symlink). After fix: both go through the helper, become
        // canonical, prefix match succeeds.
        assert!(
            ws.is_writable(&target),
            "Write should be allowed for new file under symlinked workspace"
        );
        assert!(
            ws.is_readable(&target),
            "Read should be allowed for new file under symlinked workspace"
        );
    }

    #[test]
    fn is_writable_path_outside_workspace_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = tmp.path().join("outside.txt"); // sibling of workspace

        // tempfile paths are under /tmp, which the real driver grants R+W.
        // Exclude base grants so this specifically tests workspace isolation.
        let mut ws = without_base_filesystem_grants(temp_sandbox(&format!(
            "outside_{}",
            uuid::Uuid::new_v4()
        )));
        ws.path = workspace;

        assert!(!ws.is_writable(&outside));
    }
}
