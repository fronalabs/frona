use include_dir::{Dir, include_dir};

use super::source::{ConfigEntry, parse_frontmatter};

static DEFAULTS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/config/agents");

pub fn embedded_agent_ids() -> Vec<&'static str> {
    DEFAULTS_DIR
        .dirs()
        .map(|d| d.path().file_name().unwrap().to_str().unwrap())
        .collect()
}

pub fn get_embedded_default(agent_id: &str, category: &str, name: &str) -> Option<ConfigEntry> {
    let filename = format!("{name}.md");

    let path = format!("{agent_id}/{category}/{filename}");
    if let Some(file) = DEFAULTS_DIR.get_file(&path) {
        let content = file.contents_utf8()?;
        return Some(parse_frontmatter(content));
    }

    if agent_id != "system" {
        let fallback_path = format!("system/{category}/{filename}");
        if let Some(file) = DEFAULTS_DIR.get_file(&fallback_path) {
            let content = file.contents_utf8()?;
            return Some(parse_frontmatter(content));
        }
    }

    None
}
