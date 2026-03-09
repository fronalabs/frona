use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ConfigEntry {
    pub template: String,
    pub metadata: HashMap<String, String>,
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

        let metadata: HashMap<String, String> =
            serde_yaml::from_str::<HashMap<String, serde_yaml::Value>>(yaml_str)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, v)| {
                    let s = match v {
                        serde_yaml::Value::String(s) => s,
                        serde_yaml::Value::Bool(b) => b.to_string(),
                        serde_yaml::Value::Number(n) => n.to_string(),
                        serde_yaml::Value::Null => return None,
                        _ => serde_yaml::to_string(&v).ok()?.trim().to_string(),
                    };
                    Some((k, s))
                })
                .collect();

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
            !entry.metadata.contains_key("model")
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

    #[test]
    fn test_parse_frontmatter_with_non_string_values() {
        let content = r#"---
name: weather
description: Get current weather and forecasts.
homepage: https://wttr.in/:help
extras: {"emoji":"🌤️","requires":{"bins":["curl"]}}
---

# Weather
"#;
        let entry = parse_frontmatter(content);
        assert_eq!(entry.template, "\n# Weather\n");
        assert_eq!(entry.metadata.get("name"), Some(&"weather".to_string()));
        assert_eq!(
            entry.metadata.get("description"),
            Some(&"Get current weather and forecasts.".to_string())
        );
        assert!(entry.metadata.contains_key("extras"), "nested object should be serialized as string");
    }
}
