//! Every mutating method on [`McpServerService`] enforces ownership against
//! the caller's `user_id` via [`McpServerService::load_owned`].

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use crate::auth::UserService;
use crate::auth::ephemeral_token::EphemeralTokenGuard;
use crate::auth::token::service::TokenService;
use crate::core::Principal;
use crate::core::error::AppError;
use crate::credential::keypair::service::KeyPairService;
use crate::credential::vault::models::{BindingScope, CredentialTarget};
use crate::credential::vault::service::VaultService;
use crate::tool::ToolDefinition;

use super::manager::McpManager;
use super::metadata::{RegistryPackage, RegistryServerEntry};
use super::models::{
    CachedMcpTool, CredentialBinding, McpPackage, McpRuntime, McpServer, McpServerInstall,
    McpServerStatus, McpServerUpdate, TransportConfig, sanitize_to_handle,
};
use super::registry::McpRegistryClient;
use super::repository::McpServerRepository;

#[async_trait]
pub trait PackageInstaller: Send + Sync {
    async fn install(&self, server: &McpServer) -> Result<(), AppError>;
}

pub struct SandboxedPackageInstaller {
    manager: Arc<McpManager>,
}

impl SandboxedPackageInstaller {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl PackageInstaller for SandboxedPackageInstaller {
    async fn install(&self, server: &McpServer) -> Result<(), AppError> {
        let sandbox = self.manager.build_install_sandbox(server);
        sandbox.setup()?;

        let (warmup_cmd, warmup_args) = match server.package.runtime {
            McpRuntime::Npm => {
                let pkg = format!("{}@{}", server.package.name, server.package.version);
                (
                    "npm",
                    vec!["install".to_string(), "--no-save".to_string(), pkg],
                )
            }
            McpRuntime::Pypi => {
                let pkg = format!("{}=={}", server.package.name, server.package.version);
                ("uv", vec!["tool".to_string(), "install".to_string(), pkg])
            }
            McpRuntime::Binary => return Ok(()),
        };

        let log_dir = std::path::Path::new(&server.workspace_dir).join("logs");
        std::fs::create_dir_all(&log_dir).ok();
        let log_path = log_dir.join("server.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| AppError::Tool(format!("opening {}: {e}", log_path.display())))?;
        let log_file_clone = log_file
            .try_clone()
            .map_err(|e| AppError::Tool(format!("cloning log fd: {e}")))?;

        let args_refs: Vec<&str> = warmup_args.iter().map(|s| s.as_str()).collect();
        let child = sandbox.spawn(
            warmup_cmd,
            &args_refs,
            Some(&server.workspace_dir),
            Vec::new(),
            None,
            std::process::Stdio::from(log_file),
            std::process::Stdio::from(log_file_clone),
        )?;

        let status = child
            .wait_with_output()
            .await
            .map_err(|e| AppError::Tool(format!("MCP package warm-up failed to run: {e}")))?
            .status;

        if !status.success() {
            let log_tail = crate::tool::mcp::manager::read_log_file(&log_path, 4096);
            return Err(AppError::Tool(format!(
                "MCP package warm-up for {} exited with {}: {}",
                server.package.name,
                status,
                log_tail
                    .lines()
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )));
        }

        tracing::info!(
            server_id = %server.id,
            package = %server.package.name,
            runtime = %server.package.runtime,
            "package warm-up succeeded"
        );
        Ok(())
    }
}

pub struct NoopPackageInstaller;

#[async_trait]
impl PackageInstaller for NoopPackageInstaller {
    async fn install(&self, _server: &McpServer) -> Result<(), AppError> {
        Ok(())
    }
}

pub struct McpServerService {
    repo: Arc<dyn McpServerRepository>,
    manager: Arc<McpManager>,
    registry: Arc<dyn McpRegistryClient>,
    vault: Arc<VaultService>,
    installer: Arc<dyn PackageInstaller>,
    tool_manager: Arc<crate::tool::manager::ToolManager>,
    token_service: TokenService,
    keypair_service: KeyPairService,
    user_service: UserService,
    policy_service: crate::policy::service::PolicyService,
    storage_service: crate::storage::service::StorageService,
    api_base_url: String,
    ephemeral_token_expiry_secs: u64,
}

pub struct StartResult {
    pub tools: Vec<ToolDefinition>,
}

pub struct UpdateResult {
    pub server: McpServer,
    pub restart_required: bool,
}

#[allow(clippy::too_many_arguments)]
impl McpServerService {
    pub fn new(
        repo: Arc<dyn McpServerRepository>,
        manager: Arc<McpManager>,
        registry: Arc<dyn McpRegistryClient>,
        vault: Arc<VaultService>,
        installer: Arc<dyn PackageInstaller>,
        tool_manager: Arc<crate::tool::manager::ToolManager>,
        token_service: TokenService,
        keypair_service: KeyPairService,
        user_service: UserService,
        policy_service: crate::policy::service::PolicyService,
        storage_service: crate::storage::service::StorageService,
        api_base_url: String,
        ephemeral_token_expiry_secs: u64,
    ) -> Self {
        Self {
            repo,
            manager,
            registry,
            vault,
            installer,
            tool_manager,
            token_service,
            keypair_service,
            user_service,
            policy_service,
            storage_service,
            api_base_url,
            ephemeral_token_expiry_secs,
        }
    }

    pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<McpServer>, AppError> {
        self.repo.find_by_user(user_id).await
    }

    pub async fn find_running(&self) -> Result<Vec<McpServer>, AppError> {
        self.repo.find_running().await
    }

    pub async fn find_by_id(&self, server_id: &str) -> Result<McpServer, AppError> {
        self.repo
            .find_by_id(server_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("mcp server {server_id}")))
    }

    pub async fn mark_status(
        &self,
        server_id: &str,
        status: McpServerStatus,
    ) -> Result<(), AppError> {
        let mut server = self.find_by_id(server_id).await?;
        server.status = status;
        server.updated_at = Utc::now();
        self.repo.update(&server).await?;
        Ok(())
    }

    pub async fn fetch_registry(&self, name: &str) -> Result<RegistryServerEntry, AppError> {
        self.registry.fetch(name).await
    }

    pub async fn search_registry(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RegistryServerEntry>, AppError> {
        self.registry.search(query.trim(), limit).await
    }

    async fn resolve_unique_handle(
        &self,
        user_id: &str,
        desired: &crate::core::Handle,
    ) -> Result<crate::core::Handle, AppError> {
        if self.repo.find_by_handle(user_id, desired).await?.is_none() {
            return Ok(desired.clone());
        }
        for i in 2..100u32 {
            let candidate_raw = format!("{}-{i}", desired.as_str());
            let candidate_raw = if candidate_raw.len() > 32 {
                let mut s = candidate_raw[..32].to_string();
                s = s.trim_end_matches(['-', '_']).to_string();
                s
            } else {
                candidate_raw
            };
            let Ok(candidate) = crate::core::Handle::try_new(&candidate_raw) else {
                continue;
            };
            if self
                .repo
                .find_by_handle(user_id, &candidate)
                .await?
                .is_none()
            {
                return Ok(candidate);
            }
        }
        Err(AppError::Validation(format!(
            "could not find a free mcp handle near '{}' after 99 attempts",
            desired.as_str()
        )))
    }

    pub async fn install(
        &self,
        user_id: &str,
        user_handle: &crate::core::Handle,
        req: McpServerInstall,
    ) -> Result<McpServer, AppError> {
        let entry = self.resolve_entry(&req).await?;
        let package = pick_package(&entry).ok_or_else(|| {
            AppError::Validation(
                "registry entry has no npm/pypi package with a supported transport".into(),
            )
        })?;

        validate_credential_bindings(package, &req.credentials, &req.extra_env)?;
        if let Some(ref policy) = req.sandbox_policy {
            validate_absolute_paths(&policy.read_paths)?;
            validate_absolute_paths(&policy.write_paths)?;
        }

        let handle_source = req
            .display_name_override
            .as_deref()
            .or(entry.title.as_deref())
            .unwrap_or(&entry.name);
        let desired_handle = req
            .handle
            .clone()
            .unwrap_or_else(|| sanitize_to_handle(handle_source));
        let handle = self.resolve_unique_handle(user_id, &desired_handle).await?;

        let id = crate::core::repository::new_id();
        self.verify_grants(user_id, &id, &req.credentials).await?;

        let workspace_dir = self
            .manager
            .storage()
            .mcp_workspace_path(user_handle, &handle)
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&workspace_dir).map_err(|e| {
            AppError::Tool(format!(
                "creating MCP server workspace {workspace_dir}: {e}"
            ))
        })?;

        let (runtime, command, args) = build_invocation(package)?;
        let mcp_package = McpPackage {
            runtime,
            name: package.identifier.clone(),
            version: package.version.clone().unwrap_or_else(|| "latest".into()),
        };

        let now = Utc::now();
        let server = McpServer {
            id: id.clone(),
            user_id: user_id.to_string(),
            handle,
            display_name: req
                .display_name_override
                .or(entry.title.clone())
                .unwrap_or_else(|| {
                    entry
                        .name
                        .rsplit('/')
                        .next()
                        .unwrap_or(&entry.name)
                        .to_string()
                }),
            description: Some(entry.description.clone()),
            repository_url: entry.repository.as_ref().and_then(|r| r.url.clone()),
            registry_id: Some(entry.name.clone()),
            server_info: None,
            package: mcp_package,
            command,
            args,
            env: req.extra_env,
            transports: entry
                .packages
                .iter()
                .filter_map(|p| {
                    let pkg_args = build_invocation(p).map(|(_, _, a)| a).ok()?;
                    Some(match p.transport.kind.as_str() {
                        "streamable-http" | "sse" => TransportConfig::Http {
                            args: pkg_args,
                            env: BTreeMap::from([("MCP_TRANSPORT_TYPE".into(), "http".into())]),
                            port_env_var: p
                                .environment_variables
                                .iter()
                                .find(|v| v.name.ends_with("_PORT") || v.name == "PORT")
                                .map(|v| v.name.clone()),
                            endpoint_path: p
                                .transport
                                .url
                                .as_ref()
                                .and_then(|u| u.rfind('/').map(|i| u[i..].to_string())),
                            url: None,
                        },
                        _ => TransportConfig::Stdio {
                            args: pkg_args,
                            env: Default::default(),
                        },
                    })
                })
                .collect(),
            active_transport: package.transport.kind.clone(),
            status: McpServerStatus::Installed,
            tool_cache: Vec::new(),
            workspace_dir,
            installed_at: now,
            last_started_at: None,
            updated_at: now,
        };

        let persisted = self.repo.create(&server).await?;
        self.write_bindings(user_id, &persisted.id, req.credentials)
            .await?;
        self.installer.install(&persisted).await?;
        let sandbox_policy = req.sandbox_policy.unwrap_or_default();
        self.policy_service
            .reconcile_sandbox_policy(
                user_id,
                crate::policy::reconcile::EntityRef::Mcp(persisted.id.clone()),
                &sandbox_policy,
            )
            .await?;
        Ok(persisted)
    }

    async fn write_bindings(
        &self,
        user_id: &str,
        server_id: &str,
        bindings: Vec<CredentialBinding>,
    ) -> Result<(), AppError> {
        let principal = Principal::mcp_server(server_id);
        for binding in bindings {
            self.vault
                .create_binding(
                    user_id,
                    principal.clone(),
                    &binding.env_var,
                    &binding.connection_id,
                    &binding.vault_item_id,
                    CredentialTarget::Single {
                        env_var: binding.env_var.clone(),
                        field: binding.field,
                    },
                    BindingScope::Durable,
                    None,
                )
                .await?;
        }
        Ok(())
    }

    async fn verify_grants(
        &self,
        user_id: &str,
        server_id: &str,
        bindings: &[CredentialBinding],
    ) -> Result<(), AppError> {
        let principal = Principal::mcp_server(server_id);
        for binding in bindings {
            let ok = self
                .vault
                .has_grant_for_item(
                    user_id,
                    &principal,
                    &binding.connection_id,
                    &binding.vault_item_id,
                )
                .await?;
            if !ok {
                return Err(AppError::Forbidden(format!(
                    "no grant for vault item {} in connection {} — approve it before installing",
                    binding.vault_item_id, binding.connection_id,
                )));
            }
        }
        Ok(())
    }

    pub async fn update(
        &self,
        user_id: &str,
        server_id: &str,
        req: McpServerUpdate,
    ) -> Result<UpdateResult, AppError> {
        let mut server = self.load_owned(user_id, server_id).await?;
        let was_running = matches!(server.status, McpServerStatus::Running);

        if let Some(ref policy) = req.sandbox_policy {
            validate_absolute_paths(&policy.read_paths)?;
            validate_absolute_paths(&policy.write_paths)?;
        }

        if let Some(credentials) = req.credentials {
            if let Some(id) = server.registry_id.as_deref() {
                let entry = self.registry.fetch(id).await?;
                if let Some(package) = pick_package(&entry) {
                    validate_credential_bindings(package, &credentials, &server.env)?;
                }
            }
            self.verify_grants(user_id, &server.id, &credentials)
                .await?;
            self.vault
                .delete_bindings_for_principal(user_id, &Principal::mcp_server(&server.id))
                .await?;
            self.write_bindings(user_id, &server.id, credentials)
                .await?;
        }

        if let Some(description) = req.description {
            server.description = Some(description);
        }
        if let Some(extra_env) = req.extra_env {
            server.env = extra_env;
        }
        if let Some(active_transport) = req.active_transport {
            server.active_transport = active_transport;
        }
        server.updated_at = Utc::now();
        let server = self.repo.update(&server).await?;

        if let Some(ref policy) = req.sandbox_policy {
            self.policy_service
                .reconcile_sandbox_policy(
                    user_id,
                    crate::policy::reconcile::EntityRef::Mcp(server.id.clone()),
                    policy,
                )
                .await?;
        }

        Ok(UpdateResult {
            server,
            restart_required: was_running,
        })
    }

    pub async fn uninstall(&self, user_id: &str, server_id: &str) -> Result<(), AppError> {
        let server = self.load_owned(user_id, server_id).await?;
        let _ = self.manager.stop(server_id).await;
        let principal = Principal::mcp_server(&server.id);
        self.vault
            .delete_bindings_for_principal(user_id, &principal)
            .await?;
        self.vault
            .delete_grants_for_principal(user_id, &principal)
            .await?;
        self.policy_service
            .reconcile_sandbox_policy(
                user_id,
                crate::policy::reconcile::EntityRef::Mcp(server_id.to_string()),
                &crate::policy::sandbox::SandboxPolicy::permissive(),
            )
            .await?;
        let _ = std::fs::remove_dir_all(PathBuf::from(&server.workspace_dir));
        self.repo.delete(server_id).await
    }

    pub async fn start(&self, user_id: &str, server_id: &str) -> Result<StartResult, AppError> {
        let mut server = self.load_owned(user_id, server_id).await?;
        let mut resolved_env = self.resolve_env(&server).await?;

        let user = self
            .user_service
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("user {user_id}")))?;

        let tokens_dir = self.storage_service.user_tokens_path(&user.handle);
        let token_guard = EphemeralTokenGuard::issue(
            &self.token_service,
            &self.keypair_service,
            &user,
            Principal::mcp_server(&server.id),
            self.ephemeral_token_expiry_secs,
            &tokens_dir,
        )
        .await?;

        resolved_env.insert(
            "FRONA_TOKEN_FILE".to_string(),
            token_guard.path().to_string_lossy().into_owned(),
        );
        resolved_env.insert("FRONA_API_URL".to_string(), self.api_base_url.clone());

        server.status = McpServerStatus::Starting;
        server.updated_at = Utc::now();
        self.repo.update(&server).await?;

        let tools = match self
            .manager
            .start_with_token(&server, resolved_env, Some(token_guard))
            .await
        {
            Ok(tools) => tools,
            Err(e) => {
                server.status = McpServerStatus::Failed;
                server.updated_at = Utc::now();
                let _ = self.repo.update(&server).await;
                return Err(e);
            }
        };

        server.status = McpServerStatus::Running;
        server.last_started_at = Some(Utc::now());
        server.updated_at = Utc::now();
        server.tool_cache = tools
            .iter()
            .map(|t| CachedMcpTool {
                name: strip_namespace(&t.id, server.handle.as_str()),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            })
            .collect();
        self.repo.update(&server).await?;

        if !tools.is_empty() {
            let mcp_tool = Arc::new(super::mcp_tool::McpTool::new(
                self.manager.clone(),
                server.handle.as_str(),
                tools.clone(),
            ));
            self.tool_manager
                .register_user_tool(user_id, mcp_tool)
                .await;
            self.policy_service.invalidate_cache(user_id).await;
        }

        Ok(StartResult { tools })
    }

    pub async fn stop(&self, user_id: &str, server_id: &str) -> Result<(), AppError> {
        let mut server = self.load_owned(user_id, server_id).await?;
        self.manager.stop(server_id).await?;
        server.status = McpServerStatus::Stopped;
        server.updated_at = Utc::now();
        self.repo.update(&server).await?;

        let owner_name = format!("mcp__{}", server.handle);
        self.tool_manager
            .deregister_user_tool(user_id, &owner_name)
            .await;
        self.policy_service.invalidate_cache(user_id).await;

        Ok(())
    }

    async fn load_owned(&self, user_id: &str, server_id: &str) -> Result<McpServer, AppError> {
        let server = self
            .repo
            .find_by_id(server_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("mcp server {server_id}")))?;
        if server.user_id != user_id {
            return Err(AppError::Forbidden(format!(
                "mcp server {server_id} belongs to another user"
            )));
        }
        Ok(server)
    }

    async fn resolve_entry(&self, req: &McpServerInstall) -> Result<RegistryServerEntry, AppError> {
        if let Some(manifest) = &req.manifest {
            return serde_json::from_value(manifest.clone())
                .map_err(|e| AppError::Validation(format!("invalid MCP server manifest: {e}")));
        }
        let name = req.registry_id.as_deref().ok_or_else(|| {
            AppError::Validation("install request must provide registry_id or manifest".into())
        })?;
        self.registry.fetch(name).await
    }

    async fn resolve_env(&self, server: &McpServer) -> Result<BTreeMap<String, String>, AppError> {
        let mut out = server.env.clone();
        let principal = Principal::mcp_server(&server.id);
        out.extend(
            self.vault
                .resolve_env(&server.user_id, &principal, None)
                .await?,
        );
        Ok(out)
    }
}

fn pick_package(entry: &RegistryServerEntry) -> Option<&RegistryPackage> {
    const PREFERRED_RUNTIMES: &[&str] = &["npm", "pypi"];
    const PREFERRED_TRANSPORTS: &[&str] = &["stdio", "streamable-http", "sse"];
    for runtime in PREFERRED_RUNTIMES {
        for transport in PREFERRED_TRANSPORTS {
            if let Some(p) = entry
                .packages
                .iter()
                .find(|p| p.registry_type == *runtime && p.transport.kind == *transport)
            {
                return Some(p);
            }
        }
    }
    None
}

fn build_invocation(
    package: &RegistryPackage,
) -> Result<(McpRuntime, String, Vec<String>), AppError> {
    let pinned = package
        .version
        .as_deref()
        .map(|v| format!("{}@{v}", package.identifier))
        .unwrap_or_else(|| package.identifier.clone());

    let runtime_args: Vec<String> = package
        .runtime_arguments
        .iter()
        .filter_map(|a| a.value.clone().or_else(|| a.default.clone()))
        .collect();
    let package_args: Vec<String> = package
        .package_arguments
        .iter()
        .filter_map(|a| a.value.clone().or_else(|| a.default.clone()))
        .collect();

    match package.registry_type.as_str() {
        "npm" => {
            let mut args = vec!["--yes".to_string(), pinned];
            args.extend(runtime_args);
            args.extend(package_args);
            Ok((McpRuntime::Npm, "npx".into(), args))
        }
        "pypi" => {
            let mut args = vec!["--from".to_string(), pinned, package.identifier.clone()];
            args.extend(runtime_args);
            args.extend(package_args);
            Ok((McpRuntime::Pypi, "uvx".into(), args))
        }
        other => Err(AppError::Validation(format!(
            "unsupported MCP package runtime: {other}"
        ))),
    }
}

/// Secret env vars can be satisfied by either a vault credential binding or
/// a plain value in `extra_env`. Any binding referring to an env var the
/// package does not declare is an error.
fn validate_credential_bindings(
    package: &RegistryPackage,
    bindings: &[CredentialBinding],
    extra_env: &BTreeMap<String, String>,
) -> Result<(), AppError> {
    let required: HashSet<&str> = package
        .environment_variables
        .iter()
        .filter(|v| v.is_secret)
        .map(|v| v.name.as_str())
        .collect();
    let mut provided: HashSet<&str> = bindings.iter().map(|b| b.env_var.as_str()).collect();
    for name in extra_env.keys() {
        if required.contains(name.as_str()) {
            provided.insert(name.as_str());
        }
    }

    let extraneous: Vec<&&str> = provided.difference(&required).collect();
    if !extraneous.is_empty() {
        return Err(AppError::Validation(format!(
            "binding(s) provided for env var(s) the package does not declare: {}",
            extraneous
                .into_iter()
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

fn validate_absolute_paths(paths: &[String]) -> Result<(), AppError> {
    for p in paths {
        if !crate::policy::validation::is_valid_policy_path(p) {
            return Err(AppError::Validation(format!(
                "sandbox path '{p}' must be absolute (start with /) or a user:// / agent:// URI"
            )));
        }
    }
    Ok(())
}

fn strip_namespace(tool_id: &str, slug: &str) -> String {
    let prefix = format!("mcp__{slug}__");
    tool_id.strip_prefix(&prefix).unwrap_or(tool_id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::vault::models::VaultField;
    use crate::tool::mcp::metadata::{RegistryArgument, RegistryEnvVar, RegistryTransport};

    fn pkg(registry_type: &str, transport: &str) -> RegistryPackage {
        RegistryPackage {
            registry_type: registry_type.into(),
            identifier: "@example/thing".into(),
            version: Some("1.2.3".into()),
            runtime_hint: None,
            transport: RegistryTransport {
                kind: transport.into(),
                url: None,
            },
            runtime_arguments: vec![],
            package_arguments: vec![],
            environment_variables: vec![],
        }
    }

    fn secret_env_var(name: &str) -> RegistryEnvVar {
        RegistryEnvVar {
            name: name.into(),
            description: None,
            is_required: true,
            is_secret: true,
            format: None,
        }
    }

    fn entry_with(packages: Vec<RegistryPackage>) -> RegistryServerEntry {
        RegistryServerEntry {
            name: "io.example/foo".into(),
            description: "d".into(),
            version: "1.0.0".into(),
            title: None,
            repository: None,
            website_url: None,
            packages,
            remotes: vec![],
            status: Default::default(),
            is_latest: true,
            status_message: None,
            status_changed_at: None,
            published_at: None,
            updated_at: None,
            enrichment: None,
            score: None,
        }
    }

    fn binding(env_var: &str) -> CredentialBinding {
        CredentialBinding {
            connection_id: "conn".into(),
            vault_item_id: "item".into(),
            env_var: env_var.into(),
            field: VaultField::Password,
        }
    }

    #[test]
    fn pick_package_prefers_npm_stdio_over_alternatives() {
        let entry = entry_with(vec![
            pkg("pypi", "stdio"),
            pkg("npm", "sse"),
            pkg("npm", "stdio"),
        ]);
        let chosen = pick_package(&entry).unwrap();
        assert_eq!(chosen.registry_type, "npm");
        assert_eq!(chosen.transport.kind, "stdio");
    }

    #[test]
    fn pick_package_falls_back_to_pypi_when_no_npm() {
        let entry = entry_with(vec![pkg("pypi", "stdio")]);
        assert_eq!(pick_package(&entry).unwrap().registry_type, "pypi");
    }

    #[test]
    fn pick_package_returns_none_for_oci_only() {
        assert!(pick_package(&entry_with(vec![pkg("oci", "stdio")])).is_none());
    }

    #[test]
    fn build_invocation_pins_npm_version_and_uses_npx() {
        let mut p = pkg("npm", "stdio");
        p.package_arguments = vec![RegistryArgument {
            kind: "positional".into(),
            name: None,
            value_hint: None,
            value: Some("--verbose".into()),
            default: None,
            is_required: false,
            is_repeated: false,
        }];
        let (runtime, cmd, args) = build_invocation(&p).unwrap();
        assert_eq!(runtime, McpRuntime::Npm);
        assert_eq!(cmd, "npx");
        assert_eq!(args, vec!["--yes", "@example/thing@1.2.3", "--verbose"]);
    }

    #[test]
    fn build_invocation_pypi_uses_uvx_from() {
        let (runtime, cmd, args) = build_invocation(&pkg("pypi", "stdio")).unwrap();
        assert_eq!(runtime, McpRuntime::Pypi);
        assert_eq!(cmd, "uvx");
        assert_eq!(
            args,
            vec!["--from", "@example/thing@1.2.3", "@example/thing"]
        );
    }

    #[test]
    fn build_invocation_errors_on_unsupported_runtime() {
        assert!(matches!(
            build_invocation(&pkg("oci", "stdio")),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn validate_credential_bindings_accepts_exact_match() {
        let mut p = pkg("npm", "stdio");
        p.environment_variables = vec![secret_env_var("GITHUB_TOKEN")];
        assert!(
            validate_credential_bindings(&p, &[binding("GITHUB_TOKEN")], &BTreeMap::new()).is_ok()
        );
    }

    #[test]
    fn validate_credential_bindings_allows_missing_secret() {
        let mut p = pkg("npm", "stdio");
        p.environment_variables = vec![secret_env_var("GITHUB_TOKEN")];
        assert!(validate_credential_bindings(&p, &[], &BTreeMap::new()).is_ok());
    }

    #[test]
    fn validate_credential_bindings_rejects_extraneous_binding() {
        let mut p = pkg("npm", "stdio");
        p.environment_variables = vec![secret_env_var("GITHUB_TOKEN")];
        let err = validate_credential_bindings(
            &p,
            &[binding("GITHUB_TOKEN"), binding("NOT_DECLARED")],
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)));
    }

    #[test]
    fn validate_credential_bindings_ignores_non_secret_env_vars() {
        let mut p = pkg("npm", "stdio");
        p.environment_variables = vec![
            secret_env_var("SECRET"),
            RegistryEnvVar {
                name: "PUBLIC".into(),
                description: None,
                is_required: false,
                is_secret: false,
                format: None,
            },
        ];
        assert!(validate_credential_bindings(&p, &[binding("SECRET")], &BTreeMap::new()).is_ok());
    }

    #[test]
    fn validate_absolute_paths_accepts_absolute_and_virtual() {
        assert!(validate_absolute_paths(&["/abs".into()]).is_ok());
        assert!(validate_absolute_paths(&["user://mina/foo".into()]).is_ok());
        assert!(validate_absolute_paths(&["agent://dev/output.csv".into()]).is_ok());
        assert!(matches!(
            validate_absolute_paths(&["relative/path".into()]).unwrap_err(),
            AppError::Validation(_)
        ));
    }

    #[test]
    fn strip_namespace_removes_prefix() {
        assert_eq!(
            strip_namespace("mcp__google_workspace__gmail_send", "google_workspace"),
            "gmail_send"
        );
        assert_eq!(strip_namespace("already_bare", "anything"), "already_bare");
    }
}
