use async_trait::async_trait;
use serde_json::Value;

use crate::agent::skill::resolver::SkillResolver;
use crate::error::AppError;

use super::{AgentTool, ToolDefinition, ToolOutput};

pub struct SkillTool {
    skill_resolver: SkillResolver,
    agent_id: String,
}

impl SkillTool {
    pub fn new(skill_resolver: SkillResolver, agent_id: String) -> Self {
        Self {
            skill_resolver,
            agent_id,
        }
    }
}

#[async_trait]
impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read_skill".to_string(),
            description: "Load the full content of a skill by name. Use this when the conversation is relevant to one of the available skills. Do not tell the user you are reading a skill — just silently load it and follow its instructions.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The name of the skill to read"
                    }
                },
                "required": ["name"]
            }),
        }]
    }

    async fn execute(&self, _tool_name: &str, arguments: Value) -> Result<ToolOutput, AppError> {
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Validation("Missing 'name' parameter".into()))?;

        match self.skill_resolver.resolve(&self.agent_id, name).await {
            Some(skill) => Ok(ToolOutput::text(skill.content)),
            None => Ok(ToolOutput::text(format!(
                "Skill '{name}' not found. Check the available skills in your system prompt."
            ))),
        }
    }
}
