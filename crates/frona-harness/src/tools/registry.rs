//! Registration, validated definitions and implementation lookup.

use rig_core::completion::request::ToolDefinition;
use std::collections::BTreeMap;

use super::Tool;

#[derive(Debug, thiserror::Error)]
pub enum ToolRegistrationError {
    #[error("tool name must not be blank or contain surrounding whitespace")]
    InvalidName,
    #[error("tool name '{0}' is reserved for structured submission")]
    ReservedName(String),
    #[error("tool '{0}' is already registered")]
    Duplicate(String),
    #[error("invalid tool argument schema: {0}")]
    Schema(String),
}

pub(super) fn validate_name(name: &str) -> Result<(), ToolRegistrationError> {
    if name.trim().is_empty() || name.trim() != name {
        return Err(ToolRegistrationError::InvalidName);
    }
    if name == crate::inference::provider::SUBMIT_TOOL_NAME {
        return Err(ToolRegistrationError::ReservedName(name.into()));
    }
    Ok(())
}

struct RegisteredTool<'a> {
    definition: ToolDefinition,
    tool: Box<dyn Tool + 'a>,
}

/// Registration only. The execution layer resolves proposed calls, checks
/// admission, and invokes the selected tool. An empty registry stays empty.
#[derive(Default)]
pub struct ToolRegistry<'a> {
    tools: BTreeMap<String, RegisteredTool<'a>>,
}

impl<'a> ToolRegistry<'a> {
    pub fn register<M>(
        &mut self,
        tool: impl super::IntoTool<M, Tool: 'a>,
    ) -> Result<(), ToolRegistrationError> {
        let tool = tool.into_tool()?;
        let definition = tool.definition().clone();
        validate_name(&definition.name)?;
        if self.tools.contains_key(&definition.name) {
            return Err(ToolRegistrationError::Duplicate(definition.name));
        }
        jsonschema::validator_for(&definition.parameters)
            .map_err(|error| ToolRegistrationError::Schema(error.to_string()))?;
        self.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                tool: Box::new(tool),
            },
        );
        Ok(())
    }

    pub(crate) fn extend(&mut self, other: Self) -> Result<(), ToolRegistrationError> {
        for (name, entry) in other.tools {
            if self.tools.contains_key(&name) {
                return Err(ToolRegistrationError::Duplicate(name));
            }
            self.tools.insert(name, entry);
        }
        Ok(())
    }

    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values().map(|entry| &entry.definition)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut (dyn Tool + 'a)> {
        match self.tools.get_mut(name) {
            Some(entry) => Some(entry.tool.as_mut()),
            None => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
