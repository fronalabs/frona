use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ConfigEntry {
    pub template: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Clone)]
pub struct AgentConfigSource {
    entries: HashMap<String, HashMap<String, HashMap<String, ConfigEntry>>>,
}

impl AgentConfigSource {
    pub fn load(base_dir: &str) -> Self {
        let mut entries: HashMap<String, HashMap<String, HashMap<String, ConfigEntry>>> =
            HashMap::new();

        let base = Path::new(base_dir);
        if !base.is_dir() {
            return Self { entries };
        }

        let Ok(agent_dirs) = std::fs::read_dir(base) else {
            return Self { entries };
        };

        for agent_entry in agent_dirs.flatten() {
            if !agent_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let agent_id = agent_entry.file_name().to_string_lossy().to_string();
            let agent_path = agent_entry.path();

            let Ok(category_dirs) = std::fs::read_dir(&agent_path) else {
                continue;
            };

            for cat_entry in category_dirs.flatten() {
                if !cat_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let category = cat_entry.file_name().to_string_lossy().to_string();
                let cat_path = cat_entry.path();

                let Ok(files) = std::fs::read_dir(&cat_path) else {
                    continue;
                };

                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }
                    let name = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();

                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let entry = parse_frontmatter(&content);
                        entries
                            .entry(agent_id.clone())
                            .or_default()
                            .entry(category.clone())
                            .or_default()
                            .insert(name, entry);
                    }
                }
            }
        }

        Self { entries }
    }

    pub fn agent_ids(&self) -> Vec<&str> {
        self.entries.keys().map(|s| s.as_str()).collect()
    }

    pub fn get(&self, agent_id: &str, category: &str, name: &str) -> Option<&ConfigEntry> {
        self.entries.get(agent_id)?.get(category)?.get(name)
    }
}

pub fn parse_frontmatter(content: &str) -> ConfigEntry {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return ConfigEntry {
            template: content.to_string(),
            metadata: HashMap::new(),
        };
    }

    let after_first = &trimmed[3..];
    if let Some(end_idx) = after_first.find("\n---") {
        let yaml_str = &after_first[..end_idx];
        let body = &after_first[end_idx + 4..];
        let body = body.strip_prefix('\n').unwrap_or(body);

        let metadata: HashMap<String, String> = serde_yaml::from_str(yaml_str)
            .unwrap_or_default();

        ConfigEntry {
            template: body.to_string(),
            metadata,
        }
    } else {
        ConfigEntry {
            template: content.to_string(),
            metadata: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_with_yaml() {
        let content = "---\nmodel: anthropic/claude-sonnet-4-5\n---\nHello world";
        let entry = parse_frontmatter(content);
        assert_eq!(entry.template, "Hello world");
        assert_eq!(
            entry.metadata.get("model"),
            Some(&"anthropic/claude-sonnet-4-5".to_string())
        );
    }

    #[test]
    fn test_parse_frontmatter_empty_yaml() {
        let content = "---\nmodel:\n---\nHello world";
        let entry = parse_frontmatter(content);
        assert_eq!(entry.template, "Hello world");
        assert!(
            entry.metadata.get("model").is_none()
                || entry.metadata.get("model") == Some(&"".to_string())
        );
    }

    #[test]
    fn test_parse_frontmatter_no_yaml() {
        let content = "Just plain text";
        let entry = parse_frontmatter(content);
        assert_eq!(entry.template, "Just plain text");
        assert!(entry.metadata.is_empty());
    }
}
