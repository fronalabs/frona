use super::{PromptError, PromptLoader};
use std::collections::BTreeSet;

/// A required named template and the exact invocation variables its caller supplies.
/// Default loader variables are additional context, not a substitute for these
/// invocation arguments. Template-specific semantic checks remain with the caller.
#[derive(Debug, Clone, Copy)]
pub struct PromptTemplate<'a> {
    pub name: &'a str,
    pub variables: &'a [&'a str],
}
impl<'a> PromptTemplate<'a> {
    pub const fn new(name: &'a str, variables: &'a [&'a str]) -> Self {
        Self { name, variables }
    }
    /// Render a required, nonempty prompt. Argument names follow the renderer's
    /// case-insensitive lookup; duplicate names (including case variants) fail.
    pub fn render(
        &self,
        loader: &PromptLoader,
        vars: &[(&str, &str)],
    ) -> Result<String, PromptError> {
        let expected: BTreeSet<_> = self
            .variables
            .iter()
            .map(|key| key.to_lowercase())
            .collect();
        let supplied: BTreeSet<_> = vars.iter().map(|(key, _)| key.to_lowercase()).collect();
        if expected != supplied
            || vars.len() != supplied.len()
            || self.variables.len() != expected.len()
        {
            return Err(PromptError::Variables {
                name: self.name.into(),
                expected: self.variables.iter().map(|v| (*v).into()).collect(),
                supplied: vars.iter().map(|(k, _)| (*k).into()).collect(),
            });
        }
        let rendered =
            loader
                .try_read_with_vars(self.name, vars)?
                .ok_or_else(|| PromptError::Missing {
                    name: self.name.into(),
                })?;
        if rendered.trim().is_empty() {
            return Err(PromptError::Empty {
                name: self.name.into(),
            });
        }
        Ok(rendered)
    }
}
