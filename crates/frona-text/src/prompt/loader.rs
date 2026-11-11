use super::{PromptError, render_template};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
};

/// Named templates and default variables, with optional filesystem overrides.
/// Files are read on every lookup, so changes are visible without rebuilding the
/// loader. String overrides and embedded defaults are owned by this value.
#[derive(Debug, Clone, Default)]
pub struct PromptLoader {
    directory: Option<PathBuf>,
    embedded: HashMap<String, String>,
    overrides: HashMap<String, String>,
    defaults: HashMap<String, String>,
}
impl PromptLoader {
    /// Load from a directory, preserving the server's existing lookup behavior.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self::default().with_directory(directory)
    }
    /// Supply named defaults without requiring files at runtime.
    pub fn embedded<K, V>(templates: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            embedded: templates
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            ..Self::default()
        }
    }
    pub fn with_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.directory = Some(directory.into());
        self
    }
    pub fn with_override(mut self, name: impl Into<String>, template: impl Into<String>) -> Self {
        self.overrides.insert(name.into(), template.into());
        self
    }
    pub fn with_var(mut self, key: &str, value: &str) -> Self {
        self.defaults.insert(key.to_lowercase(), value.to_string());
        self
    }
    pub fn defaults(&self) -> &HashMap<String, String> {
        &self.defaults
    }
    /// Legacy optional lookup: both missing prompts and failures return `None`.
    /// Use `try_read` when an invalid override must be reported.
    pub fn read(&self, name: &str) -> Option<String> {
        self.try_read(name).ok().flatten()
    }
    /// Optional prompts may be absent, but unreadable or invalid prompts are errors.
    pub fn try_read(&self, name: &str) -> Result<Option<String>, PromptError> {
        self.try_read_with_vars(name, &[])
    }
    pub fn try_read_with_vars(
        &self,
        name: &str,
        vars: &[(&str, &str)],
    ) -> Result<Option<String>, PromptError> {
        let Some(raw) = self.try_read_raw(name)? else {
            return Ok(None);
        };
        let mut merged = self.defaults.clone();
        for (key, value) in vars {
            merged.insert(key.to_lowercase(), (*value).to_string());
        }
        let refs = merged
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        render_template(&raw, &refs)
            .map(Some)
            .map_err(|error| match error {
                PromptError::Render { source, .. } => PromptError::Render {
                    name: Some(name.into()),
                    source,
                },
                error => error,
            })
    }
    /// Legacy optional lookup; use `try_read_with_vars` to retain failures.
    pub fn read_with_vars(&self, name: &str, vars: &[(&str, &str)]) -> Option<String> {
        self.try_read_with_vars(name, vars).ok().flatten()
    }
    /// Legacy raw lookup; use `try_read_raw` to distinguish absence from I/O errors.
    pub fn read_raw(&self, name: &str) -> Option<String> {
        self.try_read_raw(name).ok().flatten()
    }
    pub fn try_read_raw(&self, name: &str) -> Result<Option<String>, PromptError> {
        if let Some(raw) = self.overrides.get(name) {
            return Ok(Some(raw.clone()));
        }
        if let Some(directory) = &self.directory {
            match std::fs::read_to_string(directory.join(name)) {
                Ok(raw) => return Ok(Some(raw)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(PromptError::Read {
                        name: name.into(),
                        source,
                    });
                }
            }
        }
        Ok(self.embedded.get(name).cloned())
    }
    /// List immediate named templates in stable order. Like the server's original
    /// listing, inaccessible directory entries are omitted.
    pub fn list_dir(&self, dir: &str) -> Vec<String> {
        let mut paths = BTreeSet::new();
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir.trim_end_matches('/'))
        };
        for name in self.embedded.keys().chain(self.overrides.keys()) {
            if let Some(rest) = name.strip_prefix(&prefix)
                && !rest.is_empty()
                && !rest.contains('/')
            {
                paths.insert(name.clone());
            }
        }
        if let Some(directory) = &self.directory
            && let Ok(entries) = std::fs::read_dir(directory.join(dir))
        {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                    && let Some(name) = entry.file_name().to_str()
                {
                    paths.insert(format!("{prefix}{name}"));
                }
            }
        }
        paths.into_iter().collect()
    }
}
