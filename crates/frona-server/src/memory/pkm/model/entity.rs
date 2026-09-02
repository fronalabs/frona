use super::*;

pub const ENTITY_NAME_PROPERTY_IRI: &str = "https://schema.org/name";
pub const ENTITY_PATH_PROPERTY_IRI: &str = "https://schema.org/identifier";

/// Many-to-many bridge: memory (by id) ↔ entity (by path). The fact-attachment
/// layer - entities are reconstructed from their linked memories.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "knowledge_entity_source")]
pub struct KnowledgeEntitySource {
    pub id: String,
    pub user_id: String,
    pub memory_id: String,
    pub entity_path: String,
    pub created_at: DateTime<Utc>,
}

/// Database-only provenance for one scalar value or one member of an array-valued
/// entity attribute. Kept parallel to `attributes` so the public JSON/YAML shape remains
/// clean while each independently invalidatable value retains its supporting memories.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct AttributeSource {
    pub property: String,
    pub value: serde_json::Value,
    pub source_memory_ids: Vec<String>,
}

/// An entity - the unified node (was entity + playbook). `category` drives the
/// build dispatch; `kind` is the free LLM semantic label. Identity = `path`
/// (unique per user). `search_text` (`name + "\n" + description`) is the only
/// FTS-indexed field; re-derive it on every write.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "knowledge_entity")]
pub struct KnowledgeEntity {
    pub id: String,
    pub user_id: String,
    /// **Internal**: clean entity identity (`people/bob`); the Memory directory is
    /// appended at render to form the vault path, so a directory rename never
    /// rewrites the row. **External**: the note's full vault-relative path
    /// (`Work Notes/standup`) - its identity *is* its location.
    pub path: String,
    /// Agent-owned Memory entity (`Internal`) vs read-only user-note mirror
    /// (`External`). Discriminates writability, vault-path projection, search
    /// tagging, and trust. See [`EntityOrigin`].
    pub origin: EntityOrigin,
    pub category: EntityCategory,
    /// The ontology classes this entity is an instance of, as **full IRIs**.
    ///
    /// An entity is genuinely more than one thing - a person who is also an employee, a
    /// company that is also an employer - and forcing a single label made the
    /// Classify chooses a winner and discard the rest. Order is chronological: the
    /// newest type is last, which is what makes "reject the newest on a clash" a
    /// well-defined repair rather than an arbitrary one.
    ///
    /// **IRIs here, CURIEs in Markdown.** Storing the expanded form makes comparison
    /// plain string equality that cannot depend on which prefixes happen to be bound;
    /// frontmatter compacts at render, where a prefix shift is cosmetic because
    /// frontmatter is re-derived and never read back.
    pub kinds: Vec<String>,
    pub name: String,
    pub description: String,
    /// Grounded mentions establishing this entity's identity. Internal provenance only;
    /// never projected into wiki Markdown.
    pub identity_evidence: Vec<MemoryEvidence>,
    /// Internal assertion provenance; never projected into wiki frontmatter.
    pub attribute_sources: Vec<AttributeSource>,
    /// Indexed union of `attribute_sources[*].source_memory_ids`. This is a lookup
    /// projection only; the per-value records above remain authoritative.
    pub source_memory_ids: Vec<String>,
    /// The authored article body (the editable prose surface, title included; no
    /// frontmatter or `## History` - those are re-derived on render). Persisted so
    /// the on-disk `.md` is a *deterministic* projection: a missing/misplaced file is
    /// re-rendered from this + DB metadata with **no LLM call and no drift**, which is
    /// what makes crash recovery lossless (see `reconcile_files`). Written by the
    /// author/playbook stages; `MarkdownPage::parse` on read recomputes `has_title`.
    pub body: String,
    /// Exact canonical Markdown bytes paired with `rev`. Human sync and both Author
    /// stages persist this before the filesystem mirror write, so pulls, conflicts,
    /// and recovery remain correct when that write is delayed or fails.
    pub sync_content: Option<String>,
    /// Last External note revision copied to the server mirror. This stays `None`
    /// for Internal entities. A value different from `rev` means the mirror needs
    /// another write.
    pub mirrored_rev: Option<String>,
    /// Last External note revision whose derived memories committed successfully.
    /// This stays `None` for Internal entities. A value different from `rev` means
    /// extraction must run or retry.
    pub extracted_rev: Option<String>,
    /// Canonical Playbook paths selected by Playbook Author. Stored directly on the
    /// entity; no entity-link row or index is created for this advisory relationship.
    pub related_playbooks: Vec<String>,
    pub search_text: String,
    pub search_names: Vec<String>,
    pub search_name_tokens: Vec<String>,
    pub search_assertions: Vec<String>,
    /// Current-state attributes - a JSON object (stored natively).
    pub attributes: serde_json::Value,
    /// Usefulness counter (bumped by `memory_cite`); search ranking tiebreak.
    pub use_count: i64,
    /// Alternate names / abbreviations (e.g. `EXC` for "Example Cloud Services").
    /// Folded into `search_text` so the entity is findable under any of them - the
    /// cheap layer of alias-based entity resolution. A `HashSet` so uniqueness is
    /// structural (`SurrealValue` supports `HashSet`, unlike `BTreeSet`). Grows via
    /// extraction and resolver write-back.
    pub aliases: HashSet<String>,
    /// Content hash of the canonical bytes - the sync `rev` (CAS token + echo filter
    /// for `Internal`; note content hash / change-detect for `External`). The Internal
    /// filesystem mirror can temporarily lag this database-authoritative value.
    pub rev: Option<String>,
    pub updated_at: DateTime<Utc>,
    /// When the entity's exact authored projection last committed - stamped by the
    /// **author** stage, the last stage that touches an entity. Thus,
    /// `updated_at > rendered_at` means "not yet fully processed" rather than "not yet
    /// reconciled". The file mirror is repaired separately from `sync_content`.
    pub rendered_at: DateTime<Utc>,
}

/// IDs of planned task episodes whose task has a durable terminal episode.
///
/// Task lifecycle citations carry the stable task identity. This makes the projection
/// deterministic: the model extracts each lifecycle event, while the server connects
/// events that cite the same task. The planned record remains durable history.
pub fn terminal_task_plan_ids(memories: &[KnowledgeMemory]) -> HashSet<String> {
    let terminal_tasks = memories
        .iter()
        .filter(|memory| {
            !matches!(
                memory.disposition,
                Disposition::Erroneous | Disposition::Suspect
            )
        })
        .filter(|memory| {
            memory.episode.as_ref().is_some_and(|episode| {
                matches!(
                    episode.status,
                    EpisodeStatus::Occurred | EpisodeStatus::Cancelled
                )
            })
        })
        .flat_map(|memory| memory.evidence.iter())
        .filter_map(|evidence| match &evidence.source {
            EvidenceSource::TaskLifecycle { task_id, .. } => Some(task_id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();

    memories
        .iter()
        .filter(|memory| {
            memory
                .episode
                .as_ref()
                .is_some_and(|episode| episode.status == EpisodeStatus::Planned)
        })
        .filter(|memory| {
            memory
                .evidence
                .iter()
                .any(|evidence| match &evidence.source {
                    EvidenceSource::TaskLifecycle { task_id, .. } => {
                        terminal_tasks.contains(task_id)
                    }
                    _ => false,
                })
        })
        .map(|memory| memory.id.clone())
        .collect()
}

/// Split an entity's memories into `(current, history)` - the single projection rule
/// reused by author and recovery. A memory's role is **global**, read from its own
/// relations, disposition, and task lifecycle evidence:
/// - `Erroneous` / `Suspect` → excluded from **both** (never true / quarantined).
/// - `Outdated` disposition, or a `Replace` link → history (past / corrected value).
/// - a planned task episode with an occurred or cancelled episode for the same task
///   → history.
/// - a `Duplicate`/`Absorbed` link → dropped (still true, folded into the survivor -
///   excluded from both, like `Erroneous`).
/// - otherwise → current.
///
/// History-class (`Outdated` / `Replace`) wins over drop-class (`Duplicate`/
/// `Absorbed`) when a memory carries both.
pub fn classify_memories(
    memories: &[KnowledgeMemory],
) -> (Vec<&KnowledgeMemory>, Vec<&KnowledgeMemory>) {
    let mut current = Vec::new();
    let mut history = Vec::new();
    let terminal_task_plans = terminal_task_plan_ids(memories);
    for m in memories {
        if matches!(m.disposition, Disposition::Erroneous | Disposition::Suspect) {
            continue;
        }
        let has_replace = m
            .relations
            .iter()
            .any(|l| l.relation == RelationType::Replace);
        let has_drop = m
            .relations
            .iter()
            .any(|l| matches!(l.relation, RelationType::Duplicate | RelationType::Absorbed));
        if m.disposition == Disposition::Outdated
            || has_replace
            || terminal_task_plans.contains(&m.id)
        {
            history.push(m);
        } else if has_drop {
            continue;
        } else {
            current.push(m);
        }
    }
    (current, history)
}

/// A memory as a markdown bullet - its kind, then its content.
///
/// Page Author uses this in its evidence prompt, and the deterministic fallback body uses
/// the same shape when there is no model call to make.
///
/// Deliberately **not** the line `reconcile` feeds its model: that one carries the memory
/// id and its age, because that stage asks the model to cite memories back by id. Same
/// record, different job - those are two formats on purpose.
pub fn memory_bullet(m: &KnowledgeMemory) -> String {
    let evidence = memory_evidence_summary(m);
    format!("- ({:?}; evidence: {}) {}\n", m.kind, evidence, m.content)
}

pub fn memory_evidence_summary(m: &KnowledgeMemory) -> String {
    m.evidence
        .iter()
        .map(|item| {
            let source = match &item.source {
                EvidenceSource::UserMessage { .. } => "UserMessage",
                EvidenceSource::UserConfirmation { .. } => "UserConfirmation",
                EvidenceSource::AgentMessage { .. } => "AgentMessage",
                EvidenceSource::WebSearch { .. } => "WebSearch",
                EvidenceSource::WebPage { .. } => "WebPage",
                EvidenceSource::ToolResult { .. } => "ToolResult",
                EvidenceSource::TaskLifecycle { .. } => "TaskLifecycle",
                EvidenceSource::HumanEdit { .. } => "HumanEdit",
                EvidenceSource::ExternalNote { .. } => "ExternalNote",
            };
            format!("{source}/{:?}", item.strength)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Exponential recency-decay score for a short memory: `exp(-age / half_life)`, in
/// `(0, 1]`. Below `pkm_short_memory_demote_threshold` a memory is dropped (decay
/// sweep) or excluded from the short-memory block - the single home for the curve.
pub(crate) fn decay_score(age_secs: f32, half_life_secs: f32) -> f32 {
    (-(age_secs / half_life_secs)).exp()
}

/// The FTS-indexed text for an entity - `name`, `description`, and `aliases` joined
/// (aliases sorted for a deterministic string). Single source of truth
/// (re-derived on every write) so `search_entities` finds an entity under any alias.
pub fn derive_search_text(name: &str, description: &str, aliases: &HashSet<String>) -> String {
    let mut s = format!("{name}\n{description}");
    if !aliases.is_empty() {
        let mut sorted: Vec<&str> = aliases.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        s.push('\n');
        s.push_str(&sorted.join(" "));
    }
    s
}

pub fn derive_resolution_search(
    name: &str,
    aliases: &HashSet<String>,
    attributes: &serde_json::Value,
    relations: impl IntoIterator<Item = (String, String)>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut names: BTreeSet<String> = aliases
        .iter()
        .map(|value| normalize_identity_name(value))
        .filter(|value| !value.is_empty())
        .collect();
    let name = normalize_identity_name(name);
    if !name.is_empty() {
        names.insert(name);
    }
    let tokens: BTreeSet<String> = names
        .iter()
        .flat_map(|name| name.split_whitespace().map(str::to_string))
        .filter(|token| !token.is_empty())
        .collect();
    let mut assertions = BTreeSet::new();
    if let Some(attributes) = attributes.as_object() {
        for (property, value) in attributes {
            flatten_resolution_attribute(property, value, &mut assertions);
        }
    }
    for (relation, target) in relations {
        assertions.insert(serde_json::json!(["relation", relation, target]).to_string());
    }
    (
        names.into_iter().collect(),
        tokens.into_iter().collect(),
        assertions.into_iter().collect(),
    )
}

fn flatten_resolution_attribute(
    property: &str,
    value: &serde_json::Value,
    out: &mut BTreeSet<String>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                flatten_resolution_attribute(property, value, out);
            }
        }
        serde_json::Value::Null | serde_json::Value::Object(_) => {}
        value => {
            out.insert(serde_json::json!(["attribute", property, value]).to_string());
        }
    }
}

/// Whether an entity-to-entity edge was **asserted** (extracted from the transcript /
/// authored by a stage) or **inferred** (materialized by the reasoner, e.g. the
/// inverse of an asserted edge). Inferred edges are wiped and rewritten from
/// scratch every reasoning pass; asserted edges are durable. The ordinary link-upsert
/// path uses `Asserted` by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SurrealValue, Default)]
#[serde(rename_all = "snake_case")]
#[surreal(crate = "surrealdb::types", lowercase)]
pub enum LinkOrigin {
    #[default]
    Asserted,
    Inferred,
}

/// Typed entity-to-entity edge (the navigable graph). Serialized into each entity as
/// frontmatter `[[wikilinks]]` grouped by `relation`.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, Entity)]
#[surreal(crate = "surrealdb::types")]
#[entity(table = "knowledge_entity_link")]
pub struct KnowledgeEntityLink {
    pub id: String,
    pub user_id: String,
    pub from_entity_path: String,
    pub to_entity_path: String,
    pub relation: String,
    /// Internal assertion provenance; never projected into wiki frontmatter.
    pub source_memory_ids: Vec<String>,
    /// Asserted (durable) vs inferred (reasoner-materialized, rebuilt each pass).
    pub origin: LinkOrigin,
    pub created_at: DateTime<Utc>,
}

/// A search hit (projection of `knowledge_entity`), already ordered best-first.
#[derive(Debug, Clone)]
pub struct EntityHit {
    pub path: String,
    /// `Internal` (agent Memory entity) vs `External` (User Vault note) - drives the
    /// search tool's `[external]` tag and whether the path is directory-prefixed.
    pub origin: EntityOrigin,
    pub category: EntityCategory,
    /// Full IRIs, as stored. Compacted for display by whoever renders the hit.
    pub kinds: Vec<String>,
    pub name: String,
    pub description: String,
    pub aliases: HashSet<String>,
    pub search_name_tokens: Vec<String>,
    pub search_assertions: Vec<String>,
    /// Cached with search results so model `read_entity` tools need no second query.
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct RankedEntityHit {
    pub entity: EntityHit,
    pub score: f64,
    pub use_count: i64,
}

impl EntityHit {
    /// Return the body line that best explains why this page matched the query.
    /// Metadata-only matches intentionally have no snippet.
    pub fn match_snippet(&self, query: &str) -> Option<String> {
        const MAX_CHARS: usize = 200;

        let query_tokens: HashSet<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .map(str::to_lowercase)
            .collect();
        if query_tokens.is_empty() {
            return None;
        }

        let mut best: Option<(usize, usize, &str)> = None;
        for (index, line) in self.body.lines().enumerate() {
            let line = line.trim().trim_start_matches(['#', '-', '*', '>']).trim();
            if line.is_empty() {
                continue;
            }
            let line_tokens: HashSet<String> = line
                .split(|c: char| !c.is_alphanumeric())
                .filter(|token| !token.is_empty())
                .map(str::to_lowercase)
                .collect();
            let matches = query_tokens.intersection(&line_tokens).count();
            if matches > 0 && best.is_none_or(|(score, _, _)| matches > score) {
                best = Some((matches, index, line));
            }
        }

        let (_, _, line) = best?;
        let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.chars().count() <= MAX_CHARS {
            return Some(compact);
        }
        let mut snippet: String = compact.chars().take(MAX_CHARS - 1).collect();
        if let Some(last_space) = snippet.rfind(' ') {
            snippet.truncate(last_space);
        }
        snippet.push('…');
        Some(snippet)
    }
}

#[cfg(test)]
mod entity_hit_tests {
    use super::*;

    fn hit(body: &str) -> EntityHit {
        EntityHit {
            path: "notes/example".into(),
            origin: EntityOrigin::Internal,
            category: EntityCategory::Concept,
            kinds: Vec::new(),
            name: "Example".into(),
            description: String::new(),
            aliases: HashSet::new(),
            search_name_tokens: Vec::new(),
            search_assertions: Vec::new(),
            body: body.into(),
        }
    }

    #[test]
    fn match_snippet_chooses_the_line_with_the_most_query_terms() {
        let hit = hit(
            "# Operations\nPostgres is a database.\nRestart postgres with brew services restart postgresql.",
        );
        assert_eq!(
            hit.match_snippet("postgres restart").as_deref(),
            Some("Restart postgres with brew services restart postgresql.")
        );
        assert_eq!(hit.match_snippet("unrelated"), None);
    }

    #[test]
    fn match_snippet_is_capped_without_splitting_unicode() {
        let body = format!("needle {} finish", "café ".repeat(60));
        let snippet = hit(&body).match_snippet("needle").unwrap();
        assert!(snippet.chars().count() <= 200, "{snippet}");
        assert!(snippet.ends_with('…'), "{snippet}");
    }
}
