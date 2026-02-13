use serde_json::Value;

use crate::agent::prompt::PromptLoader;
use crate::agent::skill::resolver::SkillResolver;
use crate::core::error::AppError;
use frona_derive::agent_tool;

use super::{ToolContext, ToolOutput};

pub struct SkillTool {
    skill_resolver: SkillResolver,
    agent_id: String,
    prompts: PromptLoader,
}

impl SkillTool {
    pub fn new(skill_resolver: SkillResolver, agent_id: String, prompts: PromptLoader) -> Self {
        Self {
            skill_resolver,
            agent_id,
            prompts,
        }
    }
}

#[agent_tool(files("read_skill"))]
impl SkillTool {
    async fn execute(&self, _tool_name: &str, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AppError> {
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
