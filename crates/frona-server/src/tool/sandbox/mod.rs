pub mod resource_monitor;

use crate::auth::ephemeral_token::EphemeralTokenGuard;
use crate::core::{Principal, error::AppError};
pub use frona_sandbox::{
    Sandbox, SandboxFactory, canonicalize_with_unresolved_tail, node_env_vars,
};
use std::path::Path;
use std::sync::Arc;

/// Single entry point for building a fully-configured [`Sandbox`]. Owns
/// the Cedar policy + workspace + token + env machinery and exposes one
/// constructor per principal kind:
/// - [`SandboxManager::for_tool`] - agent inference tools (`CliTool`,
///   the typed file tools). Adds skill paths and a hook that issues a token only on execution.
/// - [`SandboxManager::for_command`] - command tools, including current vault bindings.
/// - [`SandboxManager::for_app`] - App processes under an agent workspace.
/// - [`SandboxManager::for_mcp`] - MCP servers in their own workspace.
///
/// Wraps a [`SandboxFactory`] internally; exposes it via
/// [`SandboxManager::factory`] for the two callers that need raw factory
/// surface (`CliTool` for `default_timeout_secs`,
/// `McpManager::build_install_sandbox` for the permissive install path).
pub struct SandboxManager {
    factory: Arc<SandboxFactory>,
    resource_manager: Arc<crate::tool::sandbox::resource_monitor::SystemResourceManager>,
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
        resource_manager: Arc<crate::tool::sandbox::resource_monitor::SystemResourceManager>,
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
            resource_manager,
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
        sandbox.prepend_env_vars(env);
        Ok(sandbox)
    }

    /// Server-owned quota manager; the portable factory only holds its monitor interface.
    pub fn resource_manager(
        &self,
    ) -> &Arc<crate::tool::sandbox::resource_monitor::SystemResourceManager> {
        &self.resource_manager
    }

    /// Shared sandbox factory, including the default command timeout.
    pub fn factory(&self) -> &SandboxFactory {
        &self.factory
    }

    /// Build a fully-configured Sandbox for an agent: Cedar policy + skill
    /// grants + workspace + ctx.file_paths + lazy execution credentials.
    /// File tools use this for path checks without resolving
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

        let sandbox = self
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
        let mut sandbox = sandbox;

        if !ctx.file_paths.is_empty() {
            sandbox = sandbox.with_write_paths(ctx.file_paths.clone());
        }

        sandbox = sandbox.with_execution_hook(Arc::new(FronaExecutionHook {
            token_service: self.token_service.clone(),
            keypair_service: self.keypair_service.clone(),
            user: ctx.user.clone(),
            principal: Principal::agent(agent_id),
            ttl_secs: self.ephemeral_token_expiry_secs,
            tokens_dir: self.storage_service.user_tokens_path(&ctx.user.handle),
            api_url: self.api_base_url.clone(),
            timezone: ctx.user.resolved_timezone(&self.server_timezone),
        }));

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

/// Frona-owned credential preparation. Each execution receives a fresh token.
struct FronaExecutionHook {
    token_service: crate::auth::token::service::TokenService,
    keypair_service: crate::credential::keypair::service::KeyPairService,
    user: crate::auth::User,
    principal: Principal,
    ttl_secs: u64,
    tokens_dir: std::path::PathBuf,
    api_url: String,
    timezone: String,
}

impl frona_sandbox::ExecutionHook for FronaExecutionHook {
    fn prepare<'a>(
        &'a self,
        _context: &'a frona_sandbox::ExecutionContext<'a>,
    ) -> frona_sandbox::HookFuture<'a> {
        Box::pin(async move {
            let token = EphemeralTokenGuard::issue(
                &self.token_service,
                &self.keypair_service,
                &self.user,
                self.principal.clone(),
                self.ttl_secs,
                &self.tokens_dir,
            )
            .await?;
            Ok(frona_sandbox::ExecutionLease::new()
                .env("TZ", self.timezone.clone())
                .env("FRONA_API_URL", self.api_url.clone())
                .env(
                    "FRONA_TOKEN_FILE",
                    token.path().to_string_lossy().into_owned(),
                )
                .read_file(token.path())
                .retain(token))
        })
    }
}
