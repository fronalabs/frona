use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::agent::config::source::parse_frontmatter;
use crate::api::repo::skills::SurrealSkillRepo;

use super::repository::SkillRepository;

#[derive(Debug, Clone)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedSkill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub path: Option<String>,
}

#[derive(Clone)]
pub struct SkillResolver {
    skill_repo: SurrealSkillRepo,
    config_dir: PathBuf,
}

impl SkillResolver {
    pub fn new(skill_repo: SurrealSkillRepo, config_dir: impl Into<PathBuf>) -> Self {
        Self {
            skill_repo,
            config_dir: config_dir.into(),
        }
    }

    pub async fn list(&self, agent_id: &str) -> Vec<SkillSummary> {
        let mut seen = HashMap::new();

        if let Ok(db_skills) = self.skill_repo.find_by_agent(Some(agent_id)).await {
            for skill in db_skills {
                seen.entry(skill.name.clone()).or_insert(SkillSummary {
                    name: skill.name,
                    description: skill.description,
                });
            }
        }

        for summary in self.scan_fs_skills(&self.config_dir.join("agents").join(agent_id).join("skills")) {
            seen.entry(summary.name.clone()).or_insert(summary);
        }

        if let Ok(db_globals) = self.skill_repo.find_by_agent(None).await {
            for skill in db_globals {
                seen.entry(skill.name.clone()).or_insert(SkillSummary {
                    name: skill.name,
                    description: skill.description,
                });
            }
        }

        for summary in self.scan_fs_skills(&self.config_dir.join("skills")) {
            seen.entry(summary.name.clone()).or_insert(summary);
        }

        seen.into_values().collect()
    }

    pub async fn resolve(&self, agent_id: &str, name: &str) -> Option<ResolvedSkill> {
        if let Ok(Some(skill)) = self.skill_repo.find_by_name(Some(agent_id), name).await {
            return Some(ResolvedSkill {
                name: skill.name,
                description: skill.description,
                content: skill.content,
                path: None,
            });
        }

        let agent_skill_dir = self.config_dir.join("agents").join(agent_id).join("skills").join(name);
        if let Some(resolved) = self.read_fs_skill(name, &agent_skill_dir) {
            return Some(resolved);
        }

        if let Ok(Some(skill)) = self.skill_repo.find_by_name(None, name).await {
            return Some(ResolvedSkill {
                name: skill.name,
                description: skill.description,
                content: skill.content,
                path: None,
            });
        }

        let global_skill_dir = self.config_dir.join("skills").join(name);
        self.read_fs_skill(name, &global_skill_dir)
    }

    pub fn skill_dir_path(&self, agent_id: &str, name: &str) -> Option<PathBuf> {
        let agent_path = self.config_dir.join("agents").join(agent_id).join("skills").join(name);
        if agent_path.join("SKILL.md").exists() {
            return Some(agent_path);
        }

        let global_path = self.config_dir.join("skills").join(name);
        if global_path.join("SKILL.md").exists() {
            return Some(global_path);
        }

        None
    }

    fn scan_fs_skills(&self, dir: &Path) -> Vec<SkillSummary> {
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
                results.push(SkillSummary {
                    name: skill_name,
                    description,
                });
            }
        }

        results
    }

    fn read_fs_skill(&self, name: &str, dir: &Path) -> Option<ResolvedSkill> {
        let skill_md = dir.join("SKILL.md");
        let content = std::fs::read_to_string(&skill_md).ok()?;
        let parsed = parse_frontmatter(&content);
        let description = parsed
            .metadata
            .get("description")
            .cloned()
            .unwrap_or_default();

        let abs_dir = std::fs::canonicalize(dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| dir.to_string_lossy().into_owned());

        let mut full_content = parsed.template.clone();
        if !full_content.ends_with('\n') {
            full_content.push('\n');
        }

        Some(ResolvedSkill {
            name: name.to_string(),
            description,
            content: full_content,
            path: Some(abs_dir),
        })
    }
}
