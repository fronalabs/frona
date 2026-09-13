use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::prompt::PromptLoader;
#[cfg(test)]
use crate::agent::skill::service::SkillService;
#[cfg(test)]
use crate::auth::token::service::TokenService;
use crate::core::error::AppError;
#[cfg(test)]
use crate::credential::keypair::service::KeyPairService;
#[cfg(test)]
use crate::policy::service::PolicyService;

use super::sandbox::SandboxManager;
use super::{AgentTool, InferenceContext, ToolDefinition, ToolOutput, parse_frontmatter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliToolConfig {
    pub name: String,
    pub description: String,
    pub program: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    pub parameters: HashMap<String, Value>,
    #[serde(default)]
    pub required: Vec<String>,
    pub timeout_secs: Option<u64>,
    /// CLI tools sharing this id are grouped under one provider in the UI.
    #[serde(default)]
    pub provider: Option<String>,
}

pub struct CliTool {
    config: CliToolConfig,
    sandbox_manager: Arc<SandboxManager>,
}

impl CliTool {
    pub fn new(config: CliToolConfig, sandbox_manager: Arc<SandboxManager>) -> Self {
        Self {
            config,
            sandbox_manager,
        }
    }

    fn substitute(template: &str, arguments: &Map<String, Value>) -> String {
        let mut result = template.to_string();
        for (key, value) in arguments {
            let placeholder = format!("${{{key}}}");
            let replacement = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            result = result.replace(&placeholder, &replacement);
        }
        result
    }
}

#[async_trait]
impl AgentTool for CliTool {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut properties = Map::new();
        for (key, schema) in &self.config.parameters {
            properties.insert(key.clone(), schema.clone());
        }

        let parameters = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": self.config.required,
        });

        vec![ToolDefinition {
            id: self.config.name.clone(),
            provider_id: self
                .config
                .provider
                .clone()
                .unwrap_or_else(|| self.config.name.clone()),
            description: self.config.description.clone(),
            parameters,
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let args_map = arguments
            .as_object()
            .ok_or_else(|| AppError::Tool("Arguments must be a JSON object".to_string()))?;

        for req in &self.config.required {
            if req == "description" {
                continue;
            }
            if !args_map.contains_key(req) {
                return Err(AppError::Tool(format!("Missing required parameter: {req}")));
            }
        }

        let substituted_args: Vec<String> = self
            .config
            .args
            .iter()
            .map(|a| Self::substitute(a, args_map))
            .collect();
        let args_refs: Vec<&str> = substituted_args.iter().map(|s| s.as_str()).collect();

        let agent_id = &ctx.agent.id;

        let sandbox = self.sandbox_manager.for_command(ctx).await?;

        let timeout = ctx
            .agent
            .sandbox_limits
            .as_ref()
            .map(|l| l.timeout_secs)
            .unwrap_or_else(|| self.sandbox_manager.factory().default_timeout_secs());

        let rm = self.sandbox_manager.factory().resource_manager();
        let (eff_cpu, eff_mem) = rm.effective_agent_limits(agent_id);

        tracing::debug!(
            agent = %agent_id,
            tool = %self.config.name,
            timeout_secs = timeout,
            max_cpu_pct = eff_cpu,
            max_memory_pct = eff_mem,
            "Executing sandboxed command"
        );

        let output = sandbox
            .execute(
                &self.config.program,
                &args_refs,
                timeout,
                None,
                None,
                Some(ctx.cancel_token.clone()),
            )
            .await?;

        let mut result = String::new();

        if output.cancelled {
            result.push_str("Process cancelled by user.\n");
        } else if output.resource_killed {
            result.push_str("Process killed: resource limit exceeded.\n");
        } else if output.timed_out {
            result.push_str(&format!("Process timed out after {timeout} seconds.\n"));
        }

        if let Some(code) = output.exit_code
            && code != 0
        {
            result.push_str(&format!("Exit code: {code}\n"));
        }

        if !output.stdout.is_empty() {
            result.push_str(&output.stdout);
        }

        if !output.stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("stderr:\n");
            result.push_str(&output.stderr);
        }

        if result.is_empty() {
            result.push_str("(no output)");
        }

        let failed = output.timed_out || output.exit_code.is_some_and(|c| c != 0);
        Ok(if failed {
            ToolOutput::error(result)
        } else {
            ToolOutput::text(result)
        })
    }
}

pub fn load_cli_tool_config(prompts: &PromptLoader, path: &str) -> Option<CliToolConfig> {
    let raw = prompts.read(path)?;
    let (yaml, body) = parse_frontmatter(&raw)?;

    let name = yaml.get("name")?.as_str()?.to_string();
    let program = yaml.get("program")?.as_str()?.to_string();

    let args: Vec<String> = yaml
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let stdin = yaml
        .get("stdin")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let timeout_secs = yaml.get("timeout_secs").and_then(|v| v.as_u64());
    let provider = yaml
        .get("provider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let parameters: HashMap<String, Value> = yaml
        .get("parameters")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or(Value::Null)))
                .collect()
        })
        .unwrap_or_default();

    let required: Vec<String> = yaml
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Some(CliToolConfig {
        name,
        description: body,
        program,
        args,
        stdin,
        parameters,
        required,
        timeout_secs,
        provider,
    })
}

pub fn load_cli_tool_configs(prompts: &PromptLoader) -> Vec<CliToolConfig> {
    let files = prompts.list_dir("tools");
    let mut configs = Vec::new();

    for path in &files {
        if let Some(config) = load_cli_tool_config(prompts, path) {
            configs.push(config);
            tracing::info!(path = %path, "Loaded CLI tool config");
        }
    }

    tracing::info!(count = configs.len(), "Loaded CLI tool configs");
    configs
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mock_token_services() -> (
        TokenService,
        KeyPairService,
        crate::credential::vault::service::VaultService,
    ) {
        use crate::auth::jwt::JwtService;
        use crate::db::repo::generic::SurrealRepo;

        let db: surrealdb::Surreal<surrealdb::engine::local::Db> =
            surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
                .await
                .unwrap();
        db.use_ns("test").use_db("test").await.unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();

        let keypair = KeyPairService::new("test-secret", Arc::new(SurrealRepo::new(db.clone())));
        let user_service = crate::auth::user_service::UserService::new(
            SurrealRepo::new(db.clone()),
            &crate::core::config::CacheConfig::default(),
        );
        let tokens = TokenService::new(
            Arc::new(SurrealRepo::new(db.clone())),
            JwtService::new(),
            user_service.clone(),
            900,
            604_800,
        );
        let storage = crate::storage::StorageService::new(&crate::core::config::Config::default());
        let vault = crate::credential::vault::service::VaultService::new(
            Arc::new(SurrealRepo::new(db.clone())),
            Arc::new(SurrealRepo::new(db.clone())),
            Arc::new(SurrealRepo::new(db.clone())),
            Arc::new(SurrealRepo::new(db.clone())),
            Arc::new(SurrealRepo::new(db.clone())),
            "test-secret",
            Default::default(),
            "/tmp/test-data".into(),
            storage,
            user_service,
            crate::credential::managed::ManagedVault::new(
                Arc::new(crate::db::repo::managed_vault::SurrealManagedVaultRepo::new(db.clone())),
                "test-secret",
                "managed".into(),
            ),
            Arc::new(crate::credential::managed::resolver::ManagedResolver::new(
                std::collections::HashMap::new(),
            )),
            crate::credential::managed::login::ManagedLoginService::registered(),
        );
        (tokens, keypair, vault)
    }

    #[test]
    fn test_substitute_placeholders() {
        let mut args = Map::new();
        args.insert(
            "command".to_string(),
            Value::String("echo hello".to_string()),
        );

        let result = CliTool::substitute("${command}", &args);
        assert_eq!(result, "echo hello");
    }

    #[test]
    fn test_substitute_no_match() {
        let args = Map::new();
        let result = CliTool::substitute("no placeholders here", &args);
        assert_eq!(result, "no placeholders here");
    }

    #[test]
    fn test_load_embedded_config() {
        let shared_prompts = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("resources")
            .join("prompts");
        let prompts = PromptLoader::new(shared_prompts);
        let configs = load_cli_tool_configs(&prompts);
        assert_eq!(configs.len(), 3);
        assert!(configs.iter().any(|c| c.name == "shell"));
        assert!(configs.iter().any(|c| c.name == "python"));
        assert!(configs.iter().any(|c| c.name == "node"));
    }

    fn mock_skill_service() -> SkillService {
        use crate::agent::skill::registry::SkillRegistryClient;
        use crate::agent::skill::resolver::SkillResolver;
        use crate::core::config::{CacheConfig, Config};

        let config = Config::default();
        let storage = crate::storage::StorageService::new(&config);
        let resolver = SkillResolver::new(
            "/tmp/frona_test_config",
            storage.clone(),
            "/tmp/frona_test_skills",
        );
        SkillService::new(
            SkillRegistryClient::default(),
            resolver,
            storage,
            "/tmp/frona_test_skills",
            &CacheConfig::default(),
        )
    }

    async fn mock_policy_service() -> PolicyService {
        use crate::db::repo::generic::SurrealRepo;
        use crate::policy::repository::PolicyRepository;
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .unwrap();
        crate::db::init::setup_schema(&db).await.unwrap();
        let schema = crate::policy::schema::build_schema();
        let repo: std::sync::Arc<dyn PolicyRepository> =
            std::sync::Arc::new(SurrealRepo::<crate::policy::models::Policy>::new(
                db.clone(),
            ));
        let tool_fixture = crate::app_state_fixture::build(&db).await;
        let tool_manager = tool_fixture.state.tool_manager.clone();
        let storage = crate::storage::StorageService::new(&crate::core::config::Config::default());
        let user_service = crate::auth::UserService::new(
            SurrealRepo::new(db),
            &crate::core::config::CacheConfig::default(),
        );
        let _ = user_service
            .create(&crate::auth::User {
                id: "test-user".into(),
                handle: crate::handle!("testuser"),
                email: "t@example.com".into(),
                name: "Test".into(),
                password_hash: String::new(),
                timezone: None,
                groups: Vec::new(),
                deactivated_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .await;
        PolicyService::new(repo, schema, tool_manager, storage, user_service)
    }

    #[tokio::test]
    async fn test_cli_tool_definitions() {
        let config = CliToolConfig {
            name: "shell".to_string(),
            description: "Execute a shell command".to_string(),
            program: "/bin/bash".to_string(),
            args: vec!["-c".to_string(), "${command}".to_string()],
            stdin: None,
            parameters: {
                let mut m = HashMap::new();
                m.insert(
                    "command".to_string(),
                    serde_json::json!({"type": "string", "description": "The command"}),
                );
                m
            },
            required: vec!["command".to_string()],
            timeout_secs: Some(30),
            provider: None,
        };

        let tool = CliTool::new(config, mock_sandbox_manager().await);
        let defs = tool.definitions();

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "shell");
    }

    async fn mock_sandbox_manager() -> Arc<SandboxManager> {
        let factory = Arc::new(crate::tool::sandbox::SandboxFactory::new(
            false,
            Arc::new(
                crate::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                    60.0, 60.0, 60.0, 60.0,
                ),
            ),
        ));
        let storage = crate::storage::StorageService::new(&crate::core::config::Config::default());
        let (tokens, keypair, vault) = mock_token_services().await;
        Arc::new(SandboxManager::new(
            factory,
            mock_policy_service().await,
            mock_skill_service(),
            storage,
            tokens,
            keypair,
            vault,
            "http://localhost".into(),
            300,
            "UTC".to_string(),
        ))
    }

    fn mock_context() -> InferenceContext {
        let broadcast = crate::chat::broadcast::BroadcastService::new();
        let event_sender = broadcast.create_event_sender("test-user", "test-chat", None);
        InferenceContext::new(
            crate::auth::User {
                id: "test-user".into(),
                handle: crate::handle!("testuser"),
                email: "test@test.com".into(),
                name: "Test".into(),
                password_hash: String::new(),
                timezone: None,
                groups: Vec::new(),
                deactivated_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            crate::agent::models::Agent {
                id: "test-agent".into(),
                user_id: "test-user".into(),
                handle: crate::handle!("test-agent"),
                name: "Test Agent".into(),
                description: String::new(),
                model_group: "primary".into(),
                enabled: true,
                skills: None,
                sandbox_limits: None,
                max_concurrent_tasks: None,
                avatar: None,
                identity: Default::default(),
                prompt: None,
                heartbeat_interval: None,
                next_heartbeat_at: None,
                heartbeat_chat_id: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            crate::chat::models::Chat {
                id: "test-chat".into(),
                user_id: "test-user".into(),
                space_id: None,
                task_id: None,
                agent_id: "test-agent".into(),
                title: None,
                archived_at: None,
                channel_id: None,
                channel_external_id: None,
                metadata: Default::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            event_sender,
            tokio_util::sync::CancellationToken::new(),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn test_cli_tool_execute_echo() {
        let config = CliToolConfig {
            name: "shell".to_string(),
            description: "Execute a shell command".to_string(),
            program: "/bin/bash".to_string(),
            args: vec!["-c".to_string(), "${command}".to_string()],
            stdin: None,
            parameters: {
                let mut m = HashMap::new();
                m.insert(
                    "command".to_string(),
                    serde_json::json!({"type": "string", "description": "The command"}),
                );
                m
            },
            required: vec!["command".to_string()],
            timeout_secs: Some(5),
            provider: None,
        };

        let tmp = std::env::temp_dir().join("frona_test_cli_tool");
        let _ = std::fs::create_dir_all(&tmp);

        let mut config_obj = crate::core::config::Config::default();
        config_obj.storage.data_dir = tmp.to_string_lossy().into_owned();
        let storage = crate::storage::StorageService::new(&config_obj);
        let (tokens, keypair, vault) = mock_token_services().await;
        let factory = Arc::new(crate::tool::sandbox::SandboxFactory::new(
            false,
            Arc::new(
                crate::tool::sandbox::driver::resource_monitor::SystemResourceManager::new(
                    60.0, 60.0, 60.0, 60.0,
                ),
            ),
        ));
        let wm = Arc::new(SandboxManager::new(
            factory,
            mock_policy_service().await,
            mock_skill_service(),
            storage,
            tokens,
            keypair,
            vault,
            "http://localhost".into(),
            300,
            "UTC".to_string(),
        ));
        let tool = CliTool::new(config, wm);
        let ctx = mock_context();

        let result = tool
            .execute(
                "shell",
                serde_json::json!({"command": "echo hello world"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.text_content().contains("hello world"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
