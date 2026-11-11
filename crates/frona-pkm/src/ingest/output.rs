use crate::model::{MemoryEvidence, MemoryKind};
/// One mention the extractor proposed, ready to commit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingEntity {
    pub path: String,
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub identity_evidence: Vec<MemoryEvidence>,
    /// Free-text-keyed properties as stated (`{"employer": "Example Corp"}`). The extractor emits
    /// **no** relations: whether a property is a literal or an edge to another entity needs
    /// the catalogue and the entity set, so the Classify stage decides it a stage later.
    ///
    /// **Merged** into an existing entity, not confined to its creation - a key the entity does
    /// not carry and whose statement no fact-memory records is added, and gets a memory
    /// mirrored. Extract is the only stage that reads the transcript, so a property it drops
    /// here is recoverable by nobody: while these were create-only, an entity first seen in one
    /// conversation could never gain a property stated in a later one.
    ///
    /// A key the entity already carries keeps its value. Extract owns *whether a key exists*;
    /// reconcile owns *what it says*, and letting a stage that never sees the entity overwrite
    /// the stage that maintains it would have the two fight every pass. The dedupe is against
    /// the fact-memories rather than the map because promotion **deletes** the key it turned
    /// into an edge - so a missing key means either "never stated" or "already an edge", and
    /// only the memory distinguishes them.
    pub attributes: serde_json::Value,
    pub attribute_evidence: std::collections::HashMap<String, Vec<MemoryEvidence>>,
}

/// Attribute candidates for an entity the extractor was explicitly told already exists.
/// The commit path is update-only: if the entity disappeared or the model supplied an
/// unknown path, this entry is ignored rather than turning an update into entity creation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingEntityUpdate {
    pub path: String,
    pub attributes: serde_json::Value,
    pub attribute_evidence: std::collections::HashMap<String, Vec<MemoryEvidence>>,
}

/// One fact the extractor proposed, with the entities it belongs to.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingMemory {
    /// Allocated before the extraction transaction so the checkpoint can reference the
    /// exact durable row written beside it.
    pub id: String,
    pub kind: MemoryKind,
    pub evidence: Vec<MemoryEvidence>,
    pub episode: Option<crate::model::Episode>,
    pub content: String,
    pub paths: Vec<String>,
}

/// Everything one transcript window produced, committed as a unit.
///
/// Extract used to write this straight out - a transaction per row - and only then
/// advance the watermark. A failure part-way left the earlier rows committed with the
/// watermark unmoved, so the next sweep re-read the same transcript and minted them
/// again; since nothing dedups memory content, the duplicates simply accumulated. Making
/// the window atomic removes the partial state rather than compensating for it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct IngestBatch {
    pub entities: Vec<PendingEntity>,
    pub entity_updates: Vec<PendingEntityUpdate>,
    pub memories: Vec<PendingMemory>,
    /// Provisional procedural-memory groups. The extraction transaction merges these
    /// values into Playbook consolidation entity rows.
    pub playbook_candidates: Vec<PendingPlaybookCandidate>,
    pub grounding_corrections: usize,
    pub grounding_items_dropped: usize,
    pub recall_result_lookups: usize,
    pub agent_evidence_no_tool_drops: usize,
    pub agent_evidence_strong_matches: usize,
    pub agent_evidence_fallback_reviews: usize,
    pub agent_evidence_fallback_retains: usize,
    pub agent_evidence_invalid_submissions: usize,
    pub agent_evidence_lookup_calls: usize,
    pub agent_evidence_terminal_drops: usize,
    pub research_coverage: crate::ResearchCoverageStats,
}

impl IngestBatch {
    /// Move another mined window into this commit batch.
    pub fn merge_from(&mut self, other: &mut Self) {
        self.entities.append(&mut other.entities);
        self.entity_updates.append(&mut other.entity_updates);
        self.memories.append(&mut other.memories);
        self.playbook_candidates
            .append(&mut other.playbook_candidates);
        self.grounding_corrections += other.grounding_corrections;
        self.grounding_items_dropped += other.grounding_items_dropped;
        self.recall_result_lookups += other.recall_result_lookups;
        self.agent_evidence_no_tool_drops += other.agent_evidence_no_tool_drops;
        self.agent_evidence_strong_matches += other.agent_evidence_strong_matches;
        self.agent_evidence_fallback_reviews += other.agent_evidence_fallback_reviews;
        self.agent_evidence_fallback_retains += other.agent_evidence_fallback_retains;
        self.agent_evidence_invalid_submissions += other.agent_evidence_invalid_submissions;
        self.agent_evidence_lookup_calls += other.agent_evidence_lookup_calls;
        self.agent_evidence_terminal_drops += other.agent_evidence_terminal_drops;
        self.research_coverage.add(&other.research_coverage);
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PendingPlaybookCandidate {
    pub id: String,
    pub path: String,
    pub name: String,
    pub description: String,
    pub source_memory_ids: std::collections::BTreeSet<String>,
}
