use super::*;

/// The per-user ontology **delta** (TBox) - the `frona:` terms this user's
/// Classify has minted or overridden on top of the shared reference base,
/// serialized as OWL functional syntax (`format = "ofn"`). Composed with the
/// bundled `OntologyRegistry` at reasoning time. `version` drives the CAS on write
/// (single-writer in the serial sweep, versioned as a backstop).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeOntology {
    pub id: String,
    pub user_id: String,
    /// The delta serialized as OWL functional syntax.
    pub owl: String,
    /// Serialization format tag (currently always `"ofn"`).
    pub format: String,
    /// Monotonic version, bumped on every committed edit (CAS token).
    pub version: i64,
    /// The **effective ontology**: the slice of the catalogue this user's knowledge
    /// base actually reasons over, as N-Triples.
    ///
    /// **Authoritative, not a cache.** It is re-cut when the vault's term set or the
    /// catalogue changes, but a refresh *merges*: a term whose source has left the
    /// catalogue keeps its last-known triples, so entities stay typed and the disjointness
    /// gate keeps firing rather than silently going quiet.
    pub effective_ontology: String,
    /// The IRIs the effective ontology was cut from - entity kinds, attribute keys, link relations, and
    /// whatever the delta references. Compared against the vault on load to decide
    /// whether a re-cut is needed.
    pub seeds: Vec<String>,
    /// Which catalogue sources the cut spans, including any that have since gone away.
    pub sources: Vec<String>,
    /// The catalogue contents it was cut against. A different fingerprint means a
    /// different catalogue, so the cut is re-taken - this is what makes an image
    /// upgrade propagate.
    pub catalog_fingerprint: String,
    pub updated_at: DateTime<Utc>,
}
