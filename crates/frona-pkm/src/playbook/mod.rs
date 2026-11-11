//! Portable playbook reasoning. The caller schedules items and commits accepted
//! resolutions or authored pages together with durable progress.
pub mod author;
pub mod resolve;
mod tools;
mod transcript;
pub use transcript::{ConversationSnapshot, Invocation, MessageRole, TranscriptMessage};

use crate::model::{KnowledgeEntity, KnowledgeMemory};
use std::collections::BTreeMap;

/// Effective knowledge visible to this phase, including accepted staged changes.
/// Every visible entity must have an entry in `memories_by_path`, even when empty.
/// Populate each entry with both attached and source-ID-referenced memories.
#[derive(Debug, Clone, Default)]
pub struct KnowledgeSnapshot {
    pub entities: Vec<KnowledgeEntity>,
    /// Redirects in the effective view, including chains from previous merges.
    pub redirects: BTreeMap<String, String>,
    pub memories_by_path: BTreeMap<String, Vec<KnowledgeMemory>>,
    /// Full, ordered source conversations keyed by opaque conversation ID.
    pub conversations: BTreeMap<String, ConversationSnapshot>,
}

#[cfg(test)]
mod tests;

impl KnowledgeSnapshot {
    pub(super) fn entity(&self, path: &str) -> Option<&KnowledgeEntity> {
        let path = crate::view::resolve_path(path, |path| {
            Ok(self.redirects.get(path).map(String::as_str))
        })
        .ok()?;
        self.entities.iter().find(|entity| entity.path == path)
    }
}
