use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;

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
}

pub struct CliTool {
    config: CliToolConfig,
    sandbox_manager: Arc<SandboxManager>,
    agent_id: String,
    network_access: bool,
    allowed_network_destinations: Vec<String>,
    skill_dirs: Vec<(String, String)>,
}

impl CliTool {
    pub fn new(
        config: CliToolConfig,
        sandbox_manager: Arc<SandboxManager>,
        agent_id: String,
        network_access: bool,
        allowed_network_destinations: Vec<String>,
    ) -> Self {
        Self {
            config,
            sandbox_manager,
            agent_id,
            network_access,
            allowed_network_destinations,
            skill_dirs: Vec::new(),
        }
    }

    pub fn with_skill_dirs(mut self, skill_dirs: Vec<(String, String)>) -> Self {
        self.skill_dirs = skill_dirs;
        self
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
            name: self.config.name.clone(),
            description: self.config.description.clone(),
            parameters,
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value, ctx: &InferenceContext) -> Result<ToolOutput, AppError> {
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

        let mut workspace = self.sandbox_manager.get_sandbox(
            &self.agent_id,
            self.network_access,
            self.allowed_network_destinations.clone(),
        ).with_skill_dirs(self.skill_dirs.clone());

        {
            let vault_vars = ctx.vault_env_vars.read().await;
            if !vault_vars.is_empty() {
                workspace = workspace.with_extra_env_vars(vault_vars.clone());
            }
        }

        let timeout = self.config.timeout_secs.unwrap_or(30);

        let output = workspace
            .execute(
                &self.config.program,
                &args_refs,
                timeout,
                None,
                None,
                None,
            )
            .await?;

        let mut result = String::new();

        if output.timed_out {
            result.push_str(&format!(
                "Process timed out after {timeout} seconds.\n"
            ));
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

        Ok(ToolOutput::text(result))
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

    let stdin = yaml.get("stdin").and_then(|v| v.as_str()).map(|s| s.to_string());
    let timeout_secs = yaml.get("timeout_secs").and_then(|v| v.as_u64());

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

    #[test]
    fn test_substitute_placeholders() {
        let mut args = Map::new();
        args.insert("command".to_string(), Value::String("echo hello".to_string()));

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
            .join("resources")
            .join("prompts");
        let prompts = PromptLoader::new(shared_prompts);
        let configs = load_cli_tool_configs(&prompts);
        assert_eq!(configs.len(), 2);
        assert!(configs.iter().any(|c| c.name == "shell"));
        assert!(configs.iter().any(|c| c.name == "python"));
    }

    #[test]
    fn test_cli_tool_definitions() {
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
        };

        let wm = Arc::new(SandboxManager::new("/tmp/test", false));
        let tool = CliTool::new(config, wm, "agent-1".to_string(), false, vec![]);
        let defs = tool.definitions();

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "shell");
    }

    fn mock_context() -> InferenceContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        InferenceContext::new(
            crate::auth::User {
                id: "test-user".into(),
                username: "testuser".into(),
                email: "test@test.com".into(),
                name: "Test".into(),
                password_hash: String::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            crate::agent::models::Agent {
                id: "test-agent".into(),
                user_id: Some("test-user".into()),
                name: "Test Agent".into(),
                description: String::new(),
                model_group: "primary".into(),
                enabled: true,
                tools: vec![],
                sandbox_config: None,
                max_concurrent_tasks: None,
                avatar: None,
                identity: Default::default(),
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
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            tx,
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
        };

        let tmp = std::env::temp_dir().join("frona_test_cli_tool");
        let _ = std::fs::create_dir_all(&tmp);

        let wm = Arc::new(SandboxManager::new(&tmp, false));
        let tool = CliTool::new(config, wm, "test-agent".to_string(), false, vec![]);
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
