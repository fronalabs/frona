use serde::{Deserialize, Serialize};

use crate::model::{
    AbsoluteTime, EpisodeStatus, EvidenceStrength, RelativeDuration, TemporalAnchor,
};

/// A mention: its characterization (name + description), free-text candidate
/// attributes/relations, and aliases. No `kind`, no CURIEs - untyped.
#[derive(Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub(super) struct Batch {
    pub(super) new_entities: Vec<NewEntity>,
    pub(super) existing_entity_updates: Vec<ExistingEntityUpdate>,
    pub(super) playbooks: Vec<NewPlaybookCandidate>,
    pub(super) memories: Vec<NewMemory>,
    pub(super) research_dispositions: Vec<ResearchDisposition>,
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct ResearchDisposition {
    pub(super) message: String,
    pub(super) result: ResearchDispositionResult,
    #[serde(default)]
    pub(super) reason: String,
    #[serde(default)]
    pub(super) claims: Vec<ResearchClaimDisposition>,
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct ResearchClaimDisposition {
    pub(super) claim: String,
    pub(super) result: ResearchDispositionResult,
    #[serde(default)]
    pub(super) contribution_ids: Vec<String>,
    #[serde(default)]
    pub(super) reason: String,
}

#[derive(Clone, Copy, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResearchDispositionResult {
    Extracted,
    NoDurableClaim,
    Duplicate,
    Unsupported,
}

/// A request-local grouping hint for procedural memories. Resolve, not extraction,
/// decides whether this becomes a new entity, merges into an existing one, or is split.
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct NewPlaybookCandidate {
    pub(super) id: String,
    pub(super) path: String,
    pub(super) name: String,
    #[serde(default)]
    pub(super) description: String,
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct NewEntity {
    /// Stable only inside this Ingest correction conversation; never persisted.
    #[serde(default)]
    #[schemars(required)]
    pub(super) id: String,
    pub(super) path: String,
    pub(super) name: String,
    /// Plain-words characterization of what this entity is.
    #[serde(default)]
    pub(super) description: String,
    #[serde(default)]
    pub(super) aliases: Vec<String>,
    #[serde(default)]
    pub(super) sources: Vec<SourceCitation>,
    #[serde(default)]
    pub(super) candidate_attributes: Vec<CandidateAttribute>,
}

/// Attributes stated about an entity already named in the extractor input. Kept separate
/// from `new_entities` so an update can never accidentally mint an entity.
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct ExistingEntityUpdate {
    pub(super) path: String,
    #[serde(default)]
    pub(super) candidate_attributes: Vec<CandidateAttribute>,
}

/// One free-text literal property directly about an entity. Relationships between
/// entities belong in multi-entity memories and are mapped to object properties later.
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct CandidateAttribute {
    #[serde(default)]
    #[schemars(required)]
    pub(super) id: String,
    pub(super) key: String,
    #[serde(default)]
    pub(super) value: String,
    #[serde(default)]
    pub(super) sources: Vec<SourceCitation>,
    #[serde(default)]
    pub(super) tool_evidence: Vec<ToolEvidenceCitation>,
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct NewMemory {
    #[serde(default)]
    #[schemars(required)]
    pub(super) id: String,
    pub(super) kind: String,
    #[serde(default)]
    pub(super) sources: Vec<SourceCitation>,
    #[serde(default)]
    pub(super) tool_evidence: Vec<ToolEvidenceCitation>,
    /// Required when `kind` is episodic and forbidden for every other kind.
    pub(super) episode: Option<EpisodeInput>,
    pub(super) content: String,
    #[serde(default)]
    pub(super) entities: Vec<String>,
    /// Required exactly once for Procedural memories and forbidden otherwise.
    pub(super) playbook: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct SourceCitation {
    pub(super) message: String,
    #[serde(default)]
    pub(super) quote: String,
    pub(super) strength: EvidenceStrength,
    /// True only when this User message explicitly confirms the immediately preceding
    /// Agent claim cited by the same candidate.
    #[serde(default)]
    pub(super) confirmation: bool,
}

/// A precise span selected from a server-generated evidence result. The opaque ID is
/// scoped to `message`; durable tool kind, call ID, query and URL never cross the model
/// boundary and are recovered from the extraction conversation's internal map.
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct ToolEvidenceCitation {
    pub(super) message: String,
    pub(super) evidence_id: String,
    #[serde(default)]
    pub(super) quote: String,
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct EpisodeInput {
    pub(super) status: EpisodeStatus,
    pub(super) anchor: TemporalAnchor,
    pub(super) duration: Option<RelativeDuration>,
    pub(super) absolute: Option<AbsoluteTime>,
}
