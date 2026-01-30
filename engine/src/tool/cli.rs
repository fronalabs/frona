use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::AppError;

use super::workspace::WorkspaceManager;
use super::{AgentTool, ToolDefinition, ToolOutput};

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
    workspace_manager: Arc<WorkspaceManager>,
    agent_id: String,
    network_access: bool,
    allowed_network_destinations: Vec<String>,
    skill_dirs: Vec<(String, String)>,
}

impl CliTool {
    pub fn new(
        config: CliToolConfig,
        workspace_manager: Arc<WorkspaceManager>,
        agent_id: String,
        network_access: bool,
        allowed_network_destinations: Vec<String>,
    ) -> Self {
        Self {
            config,
            workspace_manager,
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
            let placeholder = format!("{{{{{key}}}}}");
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

    async fn execute(&self, _tool_name: &str, arguments: Value) -> Result<ToolOutput, AppError> {
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

        let stdin_data = self
            .config
            .stdin
            .as_ref()
            .map(|tmpl| Self::substitute(tmpl, args_map));

        let workspace = self.workspace_manager.get_workspace(
            &self.agent_id,
            self.network_access,
            self.allowed_network_destinations.clone(),
        ).with_skill_dirs(self.skill_dirs.clone());

        let timeout = self.config.timeout_secs.unwrap_or(30);

        let output = workspace
            .execute(
                &self.config.program,
                &args_refs,
                stdin_data.as_deref(),
                timeout,
            )
            .await?;

        let mut result = String::new();

        if output.timed_out {
            result.push_str(&format!(
                "Process timed out after {timeout} seconds.\n"
            ));
        }

        if let Some(code) = output.exit_code {
            if code != 0 {
                result.push_str(&format!("Exit code: {code}\n"));
            }
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

const EMBEDDED_TOOLS_JSON: &str = include_str!("../../config/tools.json");

pub fn load_cli_tool_configs(user_config_path: &str) -> Vec<CliToolConfig> {
    let path = Path::new(user_config_path);
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(configs) => {
                    tracing::info!(path = %user_config_path, "Loaded tools config from user file");
                    return configs;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %user_config_path,
                        error = %e,
                        "Failed to parse user tools config, falling back to embedded"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %user_config_path,
                    error = %e,
                    "Failed to read user tools config, falling back to embedded"
                );
            }
        }
    }

    match serde_json::from_str(EMBEDDED_TOOLS_JSON) {
        Ok(configs) => {
            tracing::info!("Loaded embedded default tools config");
            configs
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse embedded tools config");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_placeholders() {
        let mut args = Map::new();
        args.insert("command".to_string(), Value::String("echo hello".to_string()));

        let result = CliTool::substitute("{{command}}", &args);
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
        let configs = load_cli_tool_configs("/nonexistent/path");
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "shell");
        assert_eq!(configs[1].name, "python");
    }

    #[test]
    fn test_cli_tool_definitions() {
        let config = CliToolConfig {
            name: "shell".to_string(),
            description: "Execute a shell command".to_string(),
            program: "/bin/bash".to_string(),
            args: vec!["-c".to_string(), "{{command}}".to_string()],
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

        let wm = Arc::new(WorkspaceManager::new("/tmp/test"));
        let tool = CliTool::new(config, wm, "agent-1".to_string(), false, vec![]);
        let defs = tool.definitions();

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "shell");
    }

    #[tokio::test]
    async fn test_cli_tool_execute_echo() {
        let config = CliToolConfig {
            name: "shell".to_string(),
            description: "Execute a shell command".to_string(),
            program: "/bin/bash".to_string(),
            args: vec!["-c".to_string(), "{{command}}".to_string()],
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

        let wm = Arc::new(WorkspaceManager::new(&tmp));
        let tool = CliTool::new(config, wm, "test-agent".to_string(), false, vec![]);

        let result = tool
            .execute(
                "shell",
                serde_json::json!({"command": "echo hello world"}),
            )
            .await
            .unwrap();

        assert!(result.text_content().contains("hello world"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
