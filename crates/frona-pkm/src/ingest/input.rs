use crate::{RecallProjection, TemporalSource, TranscriptEvidenceSource};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct EntitySummary {
    pub path: String,
    pub name: String,
}

/// A host-prepared snapshot for one extraction window. No application User,
/// Agent, Chat, or repository object crosses this interface.
#[derive(Debug, Clone)]
pub struct IngestInput {
    /// Transcript containing the same prompt-local handles as `evidence_sources`.
    pub transcript: String,
    pub owner_name: String,
    pub handle: String,
    /// The host must seed this entity before committing extraction results.
    pub self_path: String,
    pub existing_entities: Vec<EntitySummary>,
    pub evidence_sources: Vec<TranscriptEvidenceSource>,
    pub temporal_sources: Vec<TemporalSource>,
    pub recall: RecallProjection,
    pub timezone: String,
    /// Complete snapshot of previously rejected contents by normalized entity
    /// path. Contents must be trimmed and lowercased, matching server queries.
    /// Missing paths mean no known rejected facts, not an unavailable lookup.
    pub erroneous_contents: HashMap<String, HashSet<String>>,
}

impl Default for IngestInput {
    fn default() -> Self {
        Self {
            transcript: String::new(),
            owner_name: String::new(),
            handle: String::new(),
            self_path: "people/me".into(),
            existing_entities: Vec::new(),
            evidence_sources: Vec::new(),
            temporal_sources: Vec::new(),
            recall: RecallProjection::default(),
            timezone: "UTC".into(),
            erroneous_contents: HashMap::new(),
        }
    }
}
