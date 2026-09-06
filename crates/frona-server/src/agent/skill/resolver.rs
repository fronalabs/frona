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
        let mut seen = HashMap::new();

        let ws = self.storage.agent_workspace(user_handle, agent_handle);
        for name in ws.read_dir("skills") {
            if seen.contains_key(&name) {
                continue;
            }
            let skill_path = format!("skills/{name}/SKILL.md");
            if let Some(content) = ws.read(&skill_path) {
                let parsed = parse_frontmatter(&content);
                let description = parsed
                    .metadata
                    .get("description")
                    .cloned()
                    .unwrap_or_default();
                let (disable_model_invocation, argument_hint, arguments) =
                    extract_command_frontmatter(&parsed.metadata);

                let dir_path = ws
                    .resolve_path(&format!("skills/{name}"))
                    .and_then(|p| std::fs::canonicalize(&p).ok())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();

                seen.insert(
                    name.clone(),
                    Skill {
                        name,
                        description,
                        path: dir_path,
                        scope: SkillScope::Agent,
                        disable_model_invocation,
                        argument_hint,
                        arguments,
                    },
                );
            }
        }

        let user_dir = self.storage.user_skills_path(user_handle);
        for skill in self.scan_fs_skills(&user_dir, SkillScope::User) {
            if let Some(allowed) = agent_skills
                && !allowed.contains(&skill.name)
            {
                continue;
            }
            seen.entry(skill.name.clone()).or_insert(skill);
        }

        for skill in self.scan_fs_skills(&self.installed_dir, SkillScope::Shared) {
            if let Some(allowed) = agent_skills
                && !allowed.contains(&skill.name)
            {
                continue;
            }
            seen.entry(skill.name.clone()).or_insert(skill);
        }

        for skill in self.scan_fs_skills(&self.config_dir.join("skills"), SkillScope::Builtin) {
            if let Some(allowed) = agent_skills
                && !allowed.contains(&skill.name)
            {
                continue;
            }
            seen.entry(skill.name.clone()).or_insert(skill);
        }

        seen.into_values().collect()
    }

    fn scan_fs_skills(&self, dir: &Path, scope: SkillScope) -> Vec<Skill> {
        let mut results = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return results;
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }

            let skill_name = entry.file_name().to_string_lossy().to_string();
            let skill_md = entry.path().join("SKILL.md");

            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                let parsed = parse_frontmatter(&content);
                let description = parsed
                    .metadata
                    .get("description")
                    .cloned()
                    .unwrap_or_default();
                let (disable_model_invocation, argument_hint, arguments) =
                    extract_command_frontmatter(&parsed.metadata);

                let abs_dir = std::fs::canonicalize(entry.path())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| entry.path().to_string_lossy().into_owned());

                results.push(Skill {
                    scope,
                    name: skill_name,
                    description,
                    path: abs_dir,
                    disable_model_invocation,
                    argument_hint,
                    arguments,
                });
            }
        }

        results
    }
}
