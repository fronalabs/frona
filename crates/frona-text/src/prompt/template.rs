use super::{PromptError, PromptLoader, render_template};
use std::collections::{BTreeSet, HashMap};

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
        self.validate_variables(vars)?;
        let rendered =
            loader
                .try_read_with_vars(self.name, vars)?
                .ok_or_else(|| PromptError::Missing {
                    name: self.name.into(),
                })?;
        self.validate_output(rendered)
    }

    /// Render an already-loaded source with the same argument and nonempty-output
    /// contract as `render`. The template name identifies errors; no loader defaults
    /// are applied.
    pub fn render_source(
        &self,
        source: &str,
        vars: &[(&str, &str)],
    ) -> Result<String, PromptError> {
        self.validate_variables(vars)?;
        self.render_validated_source(source, vars)
    }

    /// Render with additional default context. Invocation arguments remain mandatory
    /// and override defaults using case-insensitive names.
    pub fn render_source_with_defaults(
        &self,
        source: &str,
        vars: &[(&str, &str)],
        defaults: &HashMap<String, String>,
    ) -> Result<String, PromptError> {
        self.validate_variables(vars)?;
        if defaults.is_empty() {
            return self.render_validated_source(source, vars);
        }
        let mut merged = defaults
            .iter()
            .map(|(key, value)| (key.to_lowercase(), value.as_str()))
            .collect::<HashMap<_, _>>();
        for (key, value) in vars {
            merged.insert(key.to_lowercase(), *value);
        }
        let vars = merged
            .iter()
            .map(|(key, value)| (key.as_str(), *value))
            .collect::<Vec<_>>();
        self.render_validated_source(source, &vars)
    }

    fn render_validated_source(
        &self,
        source: &str,
        vars: &[(&str, &str)],
    ) -> Result<String, PromptError> {
        let rendered = render_template(source, vars).map_err(|error| match error {
            PromptError::Render { source, .. } => PromptError::Render {
                name: Some(self.name.into()),
                source,
            },
            other => other,
        })?;
        self.validate_output(rendered)
    }

    fn validate_variables(&self, vars: &[(&str, &str)]) -> Result<(), PromptError> {
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
        Ok(())
    }

    fn validate_output(&self, rendered: String) -> Result<String, PromptError> {
        if rendered.trim().is_empty() {
            return Err(PromptError::Empty {
                name: self.name.into(),
            });
        }
        Ok(rendered)
    }
}
