use super::*;
use crate::model::new_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationEntityLifecycle {
    Pending,
    Active,
    Coalesced,
    Discarded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEntityContribution {
    pub name: String,
    pub description: String,
    pub aliases: BTreeSet<String>,
    pub attributes: serde_json::Value,
    pub attribute_evidence: BTreeMap<String, Vec<MemoryEvidence>>,
    pub source_memory_ids: BTreeSet<String>,
    pub existing_only: bool,
    pub occurrence_count: u64,
}

/// A provisional Playbook identity proposed by one extraction request. Extraction uses
/// it only as transaction input. The durable form is a Playbook consolidation entity row.
impl ConsolidationEntityLifecycle {
    pub fn searchable(self) -> bool {
        matches!(self, Self::Pending | Self::Active)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationEntityLink {
    pub relation: String,
    pub target_path: String,
    pub origin: LinkOrigin,
    pub source_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsolidationEntityProgress {
    pub classification: ClassificationProgress,
    pub classification_diagnostic: Option<serde_json::Value>,
    pub identity: IdentityProgress,
    pub reconciliation: ReconciliationProgress,
    pub playbook_resolution: PlaybookResolutionProgress,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ClassificationProgress {
    #[default]
    Pending,
    Accepted {
        decision: serde_json::Value,
    },
    Discarded {
        reason: String,
        at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum IdentityProgress {
    #[default]
    Pending,
    Distinct {
        fingerprint: String,
        evidence: serde_json::Value,
    },
    Coalesced {
        canonical_path: String,
        evidence: Option<serde_json::Value>,
    },
    Unresolved {
        fingerprint: String,
        diagnostic: serde_json::Value,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ReconciliationProgress {
    #[default]
    Pending,
    Accepted {
        promotions: serde_json::Value,
        retractions: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum PlaybookResolutionProgress {
    #[default]
    Pending,
    Accepted {
        proposal: serde_json::Value,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeConsolidationEntity {
    pub consolidation_entity_id: String,
    pub entity_id: Option<String>,
    pub consolidation_id: String,
    pub user_id: String,
    pub path: String,
    pub category: EntityCategory,
    pub lifecycle: ConsolidationEntityLifecycle,
    pub searchable: bool,
    pub canonical_path: Option<String>,
    pub contributions: Vec<PendingEntityContribution>,
    pub origin: EntityOrigin,
    pub kinds: Vec<String>,
    pub name: String,
    pub description: String,
    pub identity_evidence: Vec<MemoryEvidence>,
    pub aliases: HashSet<String>,
    pub attributes: serde_json::Value,
    pub attribute_sources: Vec<AttributeSource>,
    pub body: String,
    pub sync_content: Option<String>,
    pub mirrored_rev: Option<String>,
    pub extracted_rev: Option<String>,
    pub related_playbooks: Vec<String>,
    pub use_count: i64,
    pub rev: Option<String>,
    pub rendered_at: DateTime<Utc>,
    pub outgoing_links: Vec<ConsolidationEntityLink>,
    pub source_memory_ids: Vec<String>,
    pub search_text: String,
    pub search_names: Vec<String>,
    pub search_name_tokens: Vec<String>,
    pub search_assertions: Vec<String>,
    pub progress: ConsolidationEntityProgress,
    pub checkpoint_revision: u64,
    pub updated_at: DateTime<Utc>,
}

impl KnowledgeConsolidationEntity {
    pub fn from_committed(consolidation_id: &str, entity: KnowledgeEntity) -> Self {
        let mut row = Self::pending(
            consolidation_id,
            &entity.user_id,
            &entity.path,
            entity.category,
            Vec::new(),
            entity.source_memory_ids.iter().cloned().collect(),
        );
        row.lifecycle = ConsolidationEntityLifecycle::Active;
        row.apply_committed(entity);
        row
    }
    pub fn pending(
        consolidation_id: &str,
        user_id: &str,
        path: &str,
        category: EntityCategory,
        contributions: Vec<PendingEntityContribution>,
        source_memory_ids: BTreeSet<String>,
    ) -> Self {
        let mut row = Self {
            consolidation_entity_id: new_id(),
            entity_id: None,
            consolidation_id: consolidation_id.to_string(),
            user_id: user_id.to_string(),
            path: path.to_string(),
            category,
            lifecycle: ConsolidationEntityLifecycle::Pending,
            searchable: true,
            canonical_path: None,
            contributions,
            origin: EntityOrigin::Internal,
            kinds: Vec::new(),
            name: String::new(),
            description: String::new(),
            aliases: HashSet::new(),
            identity_evidence: Vec::new(),
            attributes: serde_json::json!({}),
            attribute_sources: Vec::new(),
            body: String::new(),
            sync_content: None,
            mirrored_rev: None,
            extracted_rev: None,
            related_playbooks: Vec::new(),
            use_count: 0,
            rev: None,
            rendered_at: minimum_datetime(),
            outgoing_links: Vec::new(),
            source_memory_ids: source_memory_ids.into_iter().collect(),
            search_text: String::new(),
            search_names: Vec::new(),
            search_name_tokens: Vec::new(),
            search_assertions: Vec::new(),
            progress: ConsolidationEntityProgress::default(),
            checkpoint_revision: 0,
            updated_at: Utc::now(),
        };
        row.rederive_search();
        row
    }
    pub fn validate(&self) -> Result<(), Error> {
        if self.searchable != self.lifecycle.searchable() {
            return Err(Error::Validation(format!(
                "pkm/consolidation_entity: lifecycle {:?} requires searchable={}",
                self.lifecycle,
                self.lifecycle.searchable()
            )));
        }
        if self.lifecycle == ConsolidationEntityLifecycle::Coalesced
            && self.canonical_path.as_deref().is_none_or(str::is_empty)
        {
            return Err(Error::Validation(
                "pkm/consolidation_entity: coalesced row requires canonical_path".into(),
            ));
        }
        if self.lifecycle != ConsolidationEntityLifecycle::Coalesced
            && self.canonical_path.is_some()
        {
            return Err(Error::Validation(
                "pkm/consolidation_entity: canonical_path is only valid for coalesced rows".into(),
            ));
        }
        Ok(())
    }
    pub fn rederive_search(&mut self) {
        if self.name.is_empty() {
            self.name = self
                .contributions
                .iter()
                .map(|c| c.name.trim())
                .find(|value| !value.is_empty())
                .unwrap_or_default()
                .to_string();
        }
        if self.description.is_empty() {
            self.description = self
                .contributions
                .iter()
                .map(|c| c.description.trim())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
        }
        if self.aliases.is_empty() {
            self.aliases = self
                .contributions
                .iter()
                .flat_map(|c| c.aliases.iter().cloned())
                .collect();
        }
        if self.name.is_empty()
            && self
                .contributions
                .iter()
                .any(|contribution| contribution.existing_only)
            && self.description.is_empty()
            && self.aliases.is_empty()
            && !self.search_text.is_empty()
        {
            self.searchable = self.lifecycle.searchable();
            self.updated_at = Utc::now();
            return;
        }
        self.search_text = derive_search_text(&self.name, &self.description, &self.aliases);
        let (names, tokens, assertions) = derive_resolution_search(
            &self.name,
            &self.aliases,
            &self.attributes,
            self.outgoing_links
                .iter()
                .map(|link| (link.relation.clone(), link.target_path.clone())),
        );
        self.search_names = names;
        self.search_name_tokens = tokens;
        self.search_assertions = assertions;
        self.searchable = self.lifecycle.searchable();
        self.updated_at = Utc::now();
    }
    pub fn apply_committed(&mut self, entity: KnowledgeEntity) {
        let updated_at = entity.updated_at;
        let rendered_at = entity.rendered_at;
        let committed_assertions = entity.search_assertions.clone();
        self.entity_id = Some(entity.id);
        self.origin = entity.origin;
        self.category = entity.category;
        self.kinds = entity.kinds;
        self.name = entity.name;
        self.description = entity.description;
        self.identity_evidence = entity.identity_evidence;
        self.attribute_sources = entity.attribute_sources;
        self.source_memory_ids = entity.source_memory_ids;
        self.body = entity.body;
        self.sync_content = entity.sync_content;
        self.mirrored_rev = entity.mirrored_rev;
        self.extracted_rev = entity.extracted_rev;
        self.related_playbooks = entity.related_playbooks;
        self.search_text = entity.search_text;
        self.search_names = entity.search_names;
        self.search_name_tokens = entity.search_name_tokens;
        self.search_assertions = entity.search_assertions;
        self.attributes = entity.attributes;
        self.use_count = entity.use_count;
        self.aliases = entity.aliases;
        self.rev = entity.rev;
        self.rederive_search();
        self.search_assertions.extend(committed_assertions);
        self.search_assertions.sort();
        self.search_assertions.dedup();
        self.updated_at = updated_at;
        self.rendered_at = rendered_at;
    }
    pub fn into_knowledge_entity(self, id: String) -> KnowledgeEntity {
        KnowledgeEntity {
            id,
            user_id: self.user_id,
            path: self.path,
            origin: self.origin,
            category: self.category,
            kinds: self.kinds,
            name: self.name,
            description: self.description,
            identity_evidence: self.identity_evidence,
            attribute_sources: self.attribute_sources,
            source_memory_ids: self.source_memory_ids,
            body: self.body,
            sync_content: self.sync_content,
            mirrored_rev: self.mirrored_rev,
            extracted_rev: self.extracted_rev,
            related_playbooks: self.related_playbooks,
            search_text: self.search_text,
            search_names: self.search_names,
            search_name_tokens: self.search_name_tokens,
            search_assertions: self.search_assertions,
            attributes: self.attributes,
            use_count: self.use_count,
            aliases: self.aliases,
            rev: self.rev,
            updated_at: self.updated_at,
            rendered_at: self.rendered_at,
        }
    }
    pub fn as_knowledge_entity(&self) -> KnowledgeEntity {
        self.clone()
            .into_knowledge_entity(self.entity_id.clone().unwrap_or_default())
    }
    pub fn effective_entity(&self) -> Option<Self> {
        self.lifecycle.searchable().then(|| self.clone())
    }
    pub fn mark_coalesced(&mut self, canonical_path: &str) {
        self.mark_coalesced_with_evidence(canonical_path, None);
    }
    pub fn mark_coalesced_with_evidence(
        &mut self,
        canonical_path: &str,
        evidence: Option<serde_json::Value>,
    ) {
        self.lifecycle = ConsolidationEntityLifecycle::Coalesced;
        self.searchable = false;
        self.canonical_path = Some(canonical_path.to_string());
        self.progress.identity = IdentityProgress::Coalesced {
            canonical_path: canonical_path.to_string(),
            evidence,
        };
        self.updated_at = Utc::now();
    }
    pub fn mark_discarded(&mut self, reason: &str) {
        self.lifecycle = ConsolidationEntityLifecycle::Discarded;
        self.searchable = false;
        self.canonical_path = None;
        self.progress.classification = ClassificationProgress::Discarded {
            reason: reason.to_string(),
            at: Utc::now(),
        };
        self.updated_at = Utc::now();
    }
}

pub(crate) fn merge_consolidation_attribute_values(
    held: &mut serde_json::Value,
    incoming: &serde_json::Value,
) {
    let incoming_values: Vec<&serde_json::Value> = match incoming {
        serde_json::Value::Array(values) => values.iter().collect(),
        value => vec![value],
    };
    for value in incoming_values {
        if held == value || held.as_array().is_some_and(|values| values.contains(value)) {
            continue;
        }
        match held {
            serde_json::Value::Array(values) => values.push(value.clone()),
            _ => {
                let first = std::mem::replace(held, serde_json::Value::Null);
                *held = serde_json::Value::Array(vec![first, value.clone()]);
            }
        }
    }
}

fn minimum_datetime() -> DateTime<Utc> {
    DateTime::<Utc>::MIN_UTC
}
pub(crate) fn normalize_identity_name(value: &str) -> String {
    value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}
