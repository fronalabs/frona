use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub enum ElementTarget<'a> {
    Selector(&'a str),
    Index(usize),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtractFormat {
    Text,
    Html,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageInfo {
    pub url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TabInfo {
    pub index: usize,
    pub url: String,
    pub title: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub text: String,
    pub href: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub tree: String,
    pub interactive_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenshotResult {
    pub path: String,
    pub size_bytes: usize,
    pub full_page: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarkdownPage {
    pub page: usize,
    pub page_count: usize,
    pub content: String,
    pub total_chars: usize,
}
