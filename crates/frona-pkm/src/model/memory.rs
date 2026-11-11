use super::*;

pub const PLAYBOOK_KIND_IRI: &str = "https://schema.org/HowTo";

/// Reserved path of the account owner's own `Person` entity. Identity is the path,
/// not a per-row flag - the KB is `user_id`-scoped, so one fixed path per user is
/// unambiguous. The system seeds it (`ensure_self_entity`) and the extractor is told
/// to route self-facts here.
pub const SELF_ENTITY_PATH: &str = "people/me";

/// What a memory asserts. Routes it to the right entity builder. Stored
/// snake_case; parsed (case-insensitively) from the extractor's label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Identity,
    Preference,
    Fact,
    Reference,
    Episodic,
    Procedural,
}

/// How directly the source supports a memory. This is an extraction judgment; the
/// grounding pass independently checks that the claimed support is actually present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    Explicit,
    Derived,
    Inferred,
}

/// Lifecycle state of a time-bounded episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeStatus {
    Planned,
    Occurred,
    Cancelled,
    Unconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemporalDirection {
    Past,
    Present,
    Future,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemporalUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemporalSemantics {
    Elapsed,
    Calendar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]

pub struct RelativeDuration {
    pub direction: TemporalDirection,
    pub amount: u32,
    pub unit: TemporalUnit,
    pub semantics: TemporalSemantics,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]

pub struct AbsoluteTime {
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
    pub hour: Option<u32>,
    pub minute: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]

pub struct Episode {
    pub status: EpisodeStatus,
    pub anchor: TemporalAnchor,
    pub duration: Option<RelativeDuration>,
    pub absolute: Option<AbsoluteTime>,
    pub resolved_start: Option<DateTime<Utc>>,
    pub resolved_end: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]

pub struct TemporalAnchor {
    pub message: String,
    pub quote: String,
}

impl MemoryKind {
    /// Parse the extractor's string label (the LLM emits a string in the
    /// extract JSON). Case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "identity" => Some(Self::Identity),
            "preference" => Some(Self::Preference),
            "fact" => Some(Self::Fact),
            "reference" => Some(Self::Reference),
            "episodic" => Some(Self::Episodic),
            "procedural" => Some(Self::Procedural),
            _ => None,
        }
    }
}

/// Entity category - the closed axis that drives the build/maintain dispatch
/// (`Playbook` → playbook maintainer; `Concept` → entity reconciler). Distinct
/// from `KnowledgeEntity.kind`, which is the free LLM-chosen semantic label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityCategory {
    Concept,
    Playbook,
}

/// A memory's disposition - how the projection treats it (deletion semantics).
/// `None`: current fact. `Outdated`: was true, world changed → demoted to
/// History (still shown as past-truth), carries `ended_at`. `Erroneous`: never
/// true → excluded from *every* projection (current AND History), suppresses
/// re-learning, and does not count as a valid superseder (so anything it
/// wrongly superseded is restored). Carries `erroneous_at`. `Suspect`: a
/// **reversible** data-quality quarantine - a fact the ontology flagged as
/// inconsistent (e.g. it forced a disjoint-class clash). Excluded from every
/// projection like `Erroneous`, but *unlike* it, `Suspect` does **not** suppress
/// re-learning (the fact may be re-minted) and can be reinstated to `None` once
/// the schema or a sibling fact is repaired. See the Classify's arbitration
/// ladder (the Assemble stage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    #[default]
    None,
    Outdated,
    Erroneous,
    Suspect,
}

/// A binary relationship between two memories, recorded on the **subordinate**
/// (`memory --relation--> to`). Drives the projection:
/// - `Replace`: the subordinate's value CHANGED - it's demoted to History.
/// - `Duplicate` / `Absorbed`: still true, folded into `to` - the subordinate is
///   dropped (not current, not History); the survivor inherits its entities.
///
/// The unary "was true, world moved on" case is `Disposition::Outdated`, not a
/// relation (it has no survivor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    Replace,
    Duplicate,
    Absorbed,
}

/// A typed link from a memory to the surviving memory that superseded it. Stored on
/// the subordinate's `relations`; `to` is a record link into `knowledge_memory`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub relation: RelationType,
    pub to: String,
    pub note: String,
}

/// One concrete source that supports a memory. Text sources retain the exact quoted span;
/// task lifecycle evidence is validated from its structured records instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    UserMessage {
        message_id: String,
        chat_id: String,
        quote: String,
    },
    UserConfirmation {
        message_id: String,
        chat_id: String,
        quote: String,
    },
    AgentMessage {
        message_id: String,
        agent_id: String,
        chat_id: String,
        quote: String,
    },
    WebSearch {
        message_id: String,
        chat_id: String,
        tool_call_id: String,
        quote: String,
        query: Option<String>,
        url: Option<String>,
    },
    WebPage {
        message_id: String,
        chat_id: String,
        tool_call_id: String,
        quote: String,
        url: Option<String>,
    },
    ToolResult {
        message_id: String,
        chat_id: String,
        tool_call_id: String,
        quote: String,
    },
    TaskLifecycle {
        message_id: String,
        chat_id: String,
        task_id: String,
    },
    HumanEdit {
        page_path: String,
        quote: String,
    },
    ExternalNote {
        note: String,
        quote: String,
    },
}

/// How one source supports one memory. Evidence is immutable and consumers inspect the
/// complete list; no aggregate strength or coarse source field is persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub strength: EvidenceStrength,
    pub source: EvidenceSource,
}

/// Whether an entity is the agent's own memory (`Internal`: writable projection of
/// memories, `path` stored clean and directory-prefixed at render) or a read-only
/// mirror of a user note (`External`: never written, `path` is the note's full
/// vault-relative location). The single discriminator the write-scope guard,
/// vault-path projection, search tagging, and trust tier branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntityOrigin {
    #[default]
    Internal,
    External,
}

/// Hot, decaying scratch memory - the only thing the agent writes (`remember`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeShortMemory {
    pub id: String,
    pub user_id: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    pub source_chat_id: Option<String>,
    pub validated: bool,
}

/// Atomic, immutable unit of memory - a fact distilled from a transcript.
/// `agent_id` + `chat_id` record where it came from. Immutable
/// and superseded-not-deleted: `relations` records the typed relations from THIS memory
/// to the survivor(s) that replaced it (see [`MemoryRelation`]), read by `classify_memories`.
/// A memory's role (current / History / dropped) is a **global** property of the
/// memory - entity-independent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeMemory {
    pub id: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
    pub kind: MemoryKind,
    /// Present exactly for `Episodic` memories.
    pub episode: Option<Episode>,
    pub content: String,
    /// Typed relations from THIS memory to the survivor(s) that superseded it
    /// (`Replace`/`Duplicate`/`Absorbed`). Empty for a live memory. Global.
    pub relations: Vec<MemoryRelation>,
    /// Deletion semantics. See [`Disposition`].
    pub disposition: Disposition,
    /// When an `Outdated` fact stopped being true (world changed).
    pub ended_at: Option<DateTime<Utc>>,
    /// Free-text annotation on the memory - currently the reason it was marked
    /// `Outdated` (rendered in the History "Past" section) or `Erroneous`. Generic
    /// so it can carry other lifecycle notes later. `None` for a plain memory.
    pub comment: Option<String>,
    /// When a fact was flagged `Erroneous` (never true).
    pub erroneous_at: Option<DateTime<Utc>>,
    /// Immutable source-specific support for this memory.
    pub evidence: Vec<MemoryEvidence>,
}
