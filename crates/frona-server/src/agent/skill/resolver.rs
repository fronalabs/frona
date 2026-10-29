use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::agent::config::parse_frontmatter;
use crate::storage::StorageService;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Builtin,
    Shared,
    User,
    Agent,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    #[serde(skip)]
    pub path: String,
    pub scope: SkillScope,
    /// SKILL.md `disable-model-invocation: true` — when set, this skill is
    /// excluded from the rendered `<available_skills>` block (model can't
    /// auto-trigger) but still shows up in the `/` dropdown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_model_invocation: bool,
    /// SKILL.md `argument-hint: "[city]"` or similar — display string shown in
    /// the `/` dropdown next to the skill name.
    pub argument_hint: Option<String>,
    /// SKILL.md `arguments: [name1, name2]` — declared names for `$<name>`
    /// substitution in the skill body. Empty if not declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
}

/// Pull the three slash-command frontmatter fields out of the parsed metadata
/// map. Centralised so both the workspace and FS scanners produce identical
/// Skill rows.
fn extract_command_frontmatter(
    metadata: &HashMap<String, String>,
) -> (bool, Option<String>, Vec<String>) {
    let disable_model_invocation = metadata
        .get("disable-model-invocation")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let argument_hint = metadata
        .get("argument-hint")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // `arguments: [a, b]` round-trips through serde_yaml's fallback as a
    // serialized YAML list (`- a\n- b`). Split lines, strip the `- ` prefix.
    let arguments = metadata
        .get("arguments")
        .map(|raw| {
            raw.lines()
                .filter_map(|l| l.trim().strip_prefix("- ").map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    (disable_model_invocation, argument_hint, arguments)
}

#[derive(Clone)]
pub struct SkillResolver {
    config_dir: PathBuf,
    installed_dir: PathBuf,
    storage: StorageService,
}

impl SkillResolver {
    pub fn new(
        config_dir: impl Into<PathBuf>,
        storage: StorageService,
        installed_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config_dir: config_dir.into(),
            installed_dir: installed_dir.into(),
            storage,
        }
    }

    pub fn installed_dir(&self) -> &Path {
        &self.installed_dir
    }

    pub fn builtin_skills_dir(&self) -> PathBuf {
        self.config_dir.join("skills")
    }

    /// Resolution order (first wins):
    /// 1. Agent Workspace FS
    /// 2. Per-user installed skills dir (data/users/{user}/skills/)
    /// 3. Server-wide installed skills dir (data/skills/) — filtered by agent_skills when Some
    /// 4. Built-in FS (resources/skills/) — filtered by agent_skills when Some
    ///
    /// None = all enabled (default), Some([]) = none, Some([...]) = specific
    pub fn list(
        &self,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        agent_skills: Option<&[String]>,
    ) -> Vec<Skill> {
        self.try_list(user_handle, agent_handle, agent_skills)
            .unwrap_or_default()
    }

    pub fn try_list(
        &self,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        agent_skills: Option<&[String]>,
    ) -> Result<Vec<Skill>, crate::core::error::AppError> {
        let ws = self.storage.agent_workspace(user_handle, agent_handle);
        let mut seen = std::collections::BTreeMap::new();
        for name in ws.try_read_dir("skills")? {
            if let Some(content) = ws.try_read(&format!("skills/{name}/SKILL.md"))? {
                let parsed = parse_frontmatter(&content);
                let (disable_model_invocation, argument_hint, arguments) =
                    extract_command_frontmatter(&parsed.metadata);
                let path = ws.resolve_path(&format!("skills/{name}")).ok_or_else(|| {
                    crate::core::error::AppError::Internal(format!("Missing skill path: {name}"))
                })?;
                let path = std::fs::canonicalize(path)
                    .map_err(|e| {
                        crate::core::error::AppError::Internal(format!("Resolve skill {name}: {e}"))
                    })?
                    .to_string_lossy()
                    .into_owned();
                seen.insert(
                    name.clone(),
                    Skill {
                        name,
                        description: parsed
                            .metadata
                            .get("description")
                            .cloned()
                            .unwrap_or_default(),
                        path,
                        scope: SkillScope::Agent,
                        disable_model_invocation,
                        argument_hint,
                        arguments,
                    },
                );
            }
        }
        let dirs = [
            (self.storage.user_skills_path(user_handle), SkillScope::User),
            (self.installed_dir.clone(), SkillScope::Shared),
            (self.config_dir.join("skills"), SkillScope::Builtin),
        ];
        for (dir, scope) in dirs {
            for skill in self.try_scan_fs_skills(&dir, scope)? {
                if scope != SkillScope::Agent
                    && agent_skills.is_some_and(|allowed| !allowed.contains(&skill.name))
                {
                    continue;
                }
                seen.entry(skill.name.clone()).or_insert(skill);
            }
        }
        Ok(seen.into_values().collect())
    }

    fn try_scan_fs_skills(
        &self,
        dir: &Path,
        scope: SkillScope,
    ) -> Result<Vec<Skill>, crate::core::error::AppError> {
        let error = |e: std::io::Error| {
            crate::core::error::AppError::Internal(format!("Read skills {}: {e}", dir.display()))
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(error(e)),
        };
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry.map_err(error)?;
            if !entry.file_type().map_err(error)?.is_dir() {
                continue;
            }
            let content = match std::fs::read_to_string(entry.path().join("SKILL.md")) {
                Ok(content) => content,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(error(e)),
            };
            let parsed = parse_frontmatter(&content);
            let (disable_model_invocation, argument_hint, arguments) =
                extract_command_frontmatter(&parsed.metadata);
            result.push(Skill {
                name: entry.file_name().to_string_lossy().into_owned(),
                description: parsed
                    .metadata
                    .get("description")
                    .cloned()
                    .unwrap_or_default(),
                path: std::fs::canonicalize(entry.path())
                    .map_err(error)?
                    .to_string_lossy()
                    .into_owned(),
                scope,
                disable_model_invocation,
                argument_hint,
                arguments,
            });
        }
        Ok(result)
    }
}
