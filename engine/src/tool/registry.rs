use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::error::AppError;

use super::{AgentTool, ToolContext, ToolDefinition, ToolOutput, ToolType};

pub struct AgentToolRegistry {
    tools: HashMap<String, Arc<dyn AgentTool>>,
    tool_name_to_owner: HashMap<String, String>,
    pub definitions: Vec<ToolDefinition>,
}

impl Default for AgentToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            tool_name_to_owner: HashMap::new(),
            definitions: Vec::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn AgentTool>) {
        let owner_name = tool.name().to_string();
        for mut def in tool.definitions() {
            self.tool_name_to_owner
                .insert(def.name.clone(), owner_name.clone());

            if let Some(props) = def
                .parameters
                .as_object_mut()
                .and_then(|obj| obj.get_mut("properties"))
                .and_then(|p| p.as_object_mut())
            {
                props.insert(
                    "description".to_string(),
                    serde_json::json!({
                        "type": "string",
                        "description": "A brief, human-readable description of what you intend to do with this tool call"
                    }),
                );
            }
            self.definitions.push(def);
        }
        self.tools.insert(owner_name, tool);
    }

    pub async fn execute(&self, tool_name: &str, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AppError> {
        let owner = self
            .tool_name_to_owner
            .get(tool_name)
            .ok_or_else(|| AppError::Tool(format!("Unknown tool: {tool_name}")))?;

        let tool = self
            .tools
            .get(owner)
            .ok_or_else(|| AppError::Tool(format!("Tool owner not found: {owner}")))?;

        tool.execute(tool_name, arguments, ctx).await
    }

    pub async fn cleanup(&self) -> Result<(), AppError> {
        for tool in self.tools.values() {
            tool.cleanup().await?;
        }
        Ok(())
    }

    pub fn is_external(&self, tool_name: &str) -> bool {
        let Some(owner_name) = self.tool_name_to_owner.get(tool_name) else {
            return false;
        };
        let Some(tool) = self.tools.get(owner_name) else {
            return false;
        };
        tool.tool_type(tool_name) == ToolType::External
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct MockTool;

    #[async_trait]
    impl AgentTool for MockTool {
        fn name(&self) -> &str {
            "mock"
        }

        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "mock_action".to_string(),
                description: "A mock action".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]
        }

        async fn execute(&self, tool_name: &str, _arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
            Ok(ToolOutput::text(format!("executed {tool_name}")))
        }
    }

    fn mock_context() -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        ToolContext {
            user: crate::models::user::User {
                id: "test-user".into(),
                email: "test@test.com".into(),
                name: "Test".into(),
                password_hash: String::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            agent: crate::agent::models::Agent {
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
            chat: crate::chat::models::Chat {
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
            event_tx: tx,
        }
    }

    #[tokio::test]
    async fn test_registry_dispatch() {
        let mut registry = AgentToolRegistry::new();
        registry.register(Arc::new(MockTool));

        let defs = &registry.definitions;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "mock_action");

        let ctx = mock_context();
        let output = registry
            .execute("mock_action", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.text_content(), "executed mock_action");
    }

    #[tokio::test]
    async fn test_registry_unknown_tool() {
        let registry = AgentToolRegistry::new();
        let ctx = mock_context();
        let result = registry.execute("nonexistent", serde_json::json!({}), &ctx).await;
        assert!(result.is_err());
    }
}
