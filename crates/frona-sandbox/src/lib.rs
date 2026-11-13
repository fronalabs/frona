#![doc = include_str!("../README.md")]
mod driver;
mod hook;
mod process;
pub use driver::{SandboxConfig, SandboxOutput, verify_sandbox};
pub use hook::{ExecutionContext, ExecutionHook, ExecutionLease, HookError, HookFuture};
pub use process::{ResourceMonitor, TrackedChild};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::driver::{create_driver, execute_sandboxed};

/// Pre-resolved at factory construction so per-call `allows()` matches
/// canonical forms without re-canonicalising on every tool call.
struct BaseFilesystemPolicy {
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

/// Creates sandboxes with shared platform selection and resource accounting.
pub struct SandboxFactory {
    driver: Arc<dyn driver::SandboxDriver>,
    shared_read_paths: Vec<String>,
    resource_monitor: Option<Arc<dyn ResourceMonitor>>,
    default_timeout_secs: u64,
    base_filesystem_policy: Arc<BaseFilesystemPolicy>,
}

impl SandboxFactory {
    pub fn new(sandbox_disabled: bool) -> Self {
        Self {
            driver: Arc::from(create_driver(sandbox_disabled)),
            shared_read_paths: Vec::new(),
            resource_monitor: None,
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

    /// Attach caller-owned process accounting. No monitoring occurs by default.
    pub fn with_resource_monitor(mut self, monitor: Arc<dyn ResourceMonitor>) -> Self {
        self.resource_monitor = Some(monitor);
        self
    }

    pub fn with_shared_read_paths(mut self, paths: Vec<String>) -> Self {
        self.shared_read_paths = paths;
        self
    }

    pub fn get_sandbox(
        &self,
        workspace: PathBuf,
        owner_id: &str,
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
            owner_id: owner_id.to_string(),
            resource_monitor: self.resource_monitor.clone(),
            execution_hook: None,
            init_venv: true,
            init_node: true,
            base_filesystem_policy: Arc::clone(&self.base_filesystem_policy),
        }
    }
}

/// Per-execution limits. Filesystem and network grants remain on the sandbox.
#[derive(Debug, Clone)]
pub struct ExecutionOptions {
    /// Zero disables the deadline.
    pub timeout_secs: u64,
    /// Output content truncation threshold per stream. Returned text may
    /// additionally contain a truncation marker. This is not a memory bound.
    pub max_output_bytes: usize,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            timeout_secs: 0,
            max_output_bytes: 512 * 1024,
        }
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
    owner_id: String,
    resource_monitor: Option<Arc<dyn ResourceMonitor>>,
    execution_hook: Option<Arc<dyn ExecutionHook>>,
    init_venv: bool,
    init_node: bool,
    /// Canonical forms of the driver-hardcoded allow-list paths. Computed
    /// once at `SandboxFactory` construction and shared via `Arc` across
    /// every `Sandbox` it produces. Used by `allows()` so the per-call
    /// check iterates over canonical forms without re-canonicalising on
    /// every tool invocation.
    base_filesystem_policy: Arc<BaseFilesystemPolicy>,
}

impl Sandbox {
    /// Install one trusted preparation hook. Replaces any previously installed hook.
    pub fn with_execution_hook(mut self, hook: Arc<dyn ExecutionHook>) -> Self {
        self.execution_hook = Some(hook);
        self
    }

    async fn prepare_execution(
        &self,
        program: &str,
        args: &[&str],
        config: &mut SandboxConfig,
    ) -> Result<ExecutionLease, Error> {
        let lease = if let Some(hook) = &self.execution_hook {
            let context = ExecutionContext {
                program,
                args,
                workspace: Path::new(&config.workspace_dir),
                working_directory: Path::new(
                    config
                        .working_dir
                        .as_deref()
                        .unwrap_or(&config.workspace_dir),
                ),
                owner_id: &self.owner_id,
            };
            hook.prepare(&context).await.map_err(Error::Hook)?
        } else {
            ExecutionLease::new()
        };
        config.env_vars.extend(lease.env.iter().cloned());
        config.additional_read_files.extend(
            lease
                .read_files
                .iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
        // Drivers require absolute filesystem rules. Match allows() by resolving
        // against the host cwd, including symlinks and not-yet-created tails.
        // Do this after the hook so per-execution file grants follow the same rule.
        for path in config
            .additional_read_paths
            .iter_mut()
            .chain(config.additional_read_files.iter_mut())
            .chain(config.additional_write_paths.iter_mut())
            .chain(config.denied_paths.iter_mut())
        {
            *path = canonicalize_with_unresolved_tail(Path::new(path))
                .to_string_lossy()
                .into_owned();
        }
        Ok(lease)
    }

    /// Prepend environment values while preserving existing values as overrides.
    pub fn prepend_env_vars(&mut self, vars: Vec<(String, String)>) {
        self.extra_env_vars.splice(0..0, vars);
    }

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

    pub fn setup(&self) -> Result<(), Error> {
        self.ensure_dir()?;
        if self.init_venv {
            self.setup_venv();
        }
        if self.init_node {
            self.setup_node_env();
        }
        Ok(())
    }

    pub fn ensure_dir(&self) -> Result<(), Error> {
        if !self.path.exists() {
            std::fs::create_dir_all(&self.path).map_err(|e| {
                Error::Tool(format!(
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

    fn base_config(&self) -> Result<SandboxConfig, Error> {
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
    pub async fn spawn(
        &self,
        program: &str,
        args: &[&str],
        working_dir: Option<&str>,
        extra_path_dirs: Vec<String>,
        stdin: Option<std::process::Stdio>,
        stdout: std::process::Stdio,
        stderr: std::process::Stdio,
    ) -> Result<TrackedChild, Error> {
        let mut config = self.base_config()?;

        config.additional_path_dirs.extend(extra_path_dirs);

        if let Some(wd) = working_dir {
            let canonical_wd = std::fs::canonicalize(wd)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| wd.to_string());
            config.working_dir = Some(canonical_wd);
        }

        let lease = self.prepare_execution(program, args, &mut config).await?;
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
            .map_err(|e| Error::Tool(format!("Failed to spawn process: {e}")))?;

        Ok(TrackedChild::new(
            child,
            self.resource_monitor.clone(),
            &self.owner_id,
            lease,
        ))
    }

    pub async fn execute(
        &self,
        program: &str,
        args: &[&str],
        timeout_secs: u64,
        on_stdout: Option<mpsc::Sender<String>>,
        stdin_rx: Option<mpsc::Receiver<String>>,
        cancel_token: Option<CancellationToken>,
    ) -> Result<SandboxOutput, Error> {
        self.execute_with_options(
            program,
            args,
            ExecutionOptions {
                timeout_secs,
                ..Default::default()
            },
            on_stdout,
            stdin_rx,
            cancel_token,
        )
        .await
    }

    /// Execute with explicit deadline and retained-output limits.
    pub async fn execute_with_options(
        &self,
        program: &str,
        args: &[&str],
        options: ExecutionOptions,
        on_stdout: Option<mpsc::Sender<String>>,
        stdin_rx: Option<mpsc::Receiver<String>>,
        cancel_token: Option<CancellationToken>,
    ) -> Result<SandboxOutput, Error> {
        let mut config = self.base_config()?;
        config.timeout_secs = options.timeout_secs;
        config.max_output_bytes = options.max_output_bytes;

        let lease = tokio::select! {
            biased;
            _ = async { if let Some(token) = &cancel_token { token.cancelled().await } else { std::future::pending::<()>().await } } => {
                return Ok(SandboxOutput { stdout: String::new(), stderr: String::new(), exit_code: None, timed_out: false, cancelled: true, resource_killed: false });
            }
            lease = self.prepare_execution(program, args, &mut config) => lease?,
        };
        execute_sandboxed(
            &*self.driver,
            program,
            args,
            &config,
            on_stdout,
            stdin_rx,
            cancel_token,
            self.resource_monitor.clone(),
            Some(&self.owner_id),
            lease,
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
            owner_id: "test".to_string(),
            resource_monitor: None,
            execution_hook: None,
            init_venv: true,
            init_node: true,
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

    #[cfg(target_os = "linux")]
    struct CheckFilesystemRules;

    #[cfg(target_os = "linux")]
    impl driver::SandboxDriver for CheckFilesystemRules {
        fn driver_id(&self) -> &'static str {
            "check-syd-rules"
        }

        fn sandboxed_command(
            &self,
            program: &str,
            args: &[&str],
            config: &SandboxConfig,
        ) -> Result<std::process::Command, Error> {
            let command = driver::syd::SydDriver::new().sandboxed_command(program, args, config)?;
            let rules: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_string_lossy())
                .collect();
            for (prefix, path, suffix) in [
                ("allow/read+", "random_number.py", "/***"),
                ("allow/read+", "missing-grants/file", ""),
                ("allow/read+", "missing-grants/hook-token", ""),
                ("allow/read+", "missing-grants/output", "/***"),
                ("allow/write+", "missing-grants/output", "/***"),
                ("allow/write+", "random_number.py", "/***"),
                ("mask+", "missing-grants/secret", "/***"),
            ] {
                let absolute = canonicalize_with_unresolved_tail(Path::new(path));
                assert!(absolute.is_absolute());
                let expected = format!("{prefix}{}{suffix}", absolute.display());
                assert!(
                    rules.iter().any(|rule| rule == &expected),
                    "missing {expected}: {rules:?}"
                );
                assert!(
                    !rules
                        .iter()
                        .any(|rule| rule == &format!("{prefix}{path}{suffix}"))
                );
            }
            // Exact-file grants must not become directory or sibling grants.
            assert_eq!(config.additional_read_files.len(), 2);
            assert_eq!(config.additional_read_paths.len(), 1);
            driver::noop::NoopDriver.sandboxed_command(program, args, config)
        }
    }

    #[cfg(target_os = "linux")]
    struct RelativeFileHook;

    #[cfg(target_os = "linux")]
    impl ExecutionHook for RelativeFileHook {
        fn prepare<'a>(&'a self, _: &'a ExecutionContext<'a>) -> HookFuture<'a> {
            Box::pin(async { Ok(ExecutionLease::new().read_file("missing-grants/hook-token")) })
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn relative_filesystem_rules_are_absolute_for_execute_and_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let mut sandbox = SandboxFactory::new(true)
            .with_shared_read_paths(vec!["random_number.py".into()])
            .get_sandbox(directory.path().join("workspace"), "test", false, vec![])
            .without_venv()
            .without_node()
            .with_read_files(vec!["missing-grants/file".into()])
            .with_write_paths(vec![
                "missing-grants/output".into(),
                "random_number.py".into(),
            ])
            .with_denied_paths(vec!["missing-grants/secret".into()])
            .with_execution_hook(Arc::new(RelativeFileHook));
        sandbox.driver = Arc::new(CheckFilesystemRules);
        let output = sandbox
            .execute("true", &[], 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(0));
        let mut child = sandbox
            .spawn(
                "true",
                &[],
                None,
                vec![],
                None,
                std::process::Stdio::null(),
                std::process::Stdio::null(),
            )
            .await
            .unwrap();
        assert!(child.wait().await.unwrap().success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execution_rules_resolve_symlinked_parents_with_missing_tails() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let path = link.join("not-created/file").to_string_lossy().into_owned();
        let expected = std::fs::canonicalize(&real)
            .unwrap()
            .join("not-created/file")
            .to_string_lossy()
            .into_owned();
        let sandbox = SandboxFactory::new(true)
            .get_sandbox(directory.path().join("workspace"), "test", false, vec![])
            .without_venv()
            .without_node()
            .with_read_paths(vec![path.clone()])
            .with_read_files(vec![path.clone()])
            .with_write_paths(vec![path.clone()])
            .with_denied_paths(vec![path]);
        let mut config = sandbox.base_config().unwrap();
        let _lease = sandbox
            .prepare_execution("true", &[], &mut config)
            .await
            .unwrap();
        assert_eq!(
            config.additional_read_paths,
            std::slice::from_ref(&expected)
        );
        assert_eq!(
            config.additional_read_files,
            std::slice::from_ref(&expected)
        );
        assert_eq!(
            config.additional_write_paths,
            std::slice::from_ref(&expected)
        );
        assert_eq!(config.denied_paths, [expected]);
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
            owner_id: "test".to_string(),
            resource_monitor: None,
            execution_hook: None,
            init_venv: true,
            init_node: true,
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Execution hook failed: {0}")]
    Hook(#[source] HookError),
    #[error("{0}")]
    Tool(String),
}
