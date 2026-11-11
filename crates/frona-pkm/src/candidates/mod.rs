use std::collections::{BTreeSet, HashSet};

use crate::Error;
use crate::GraphSnapshot;
use crate::model::EntityHit;
use crate::model::{EntityCategory, KnowledgeConsolidationEntity, normalize_identity_name};

const IDENTITY_SIMILARITY_THRESHOLD: f64 = 0.78;
pub const RESOLUTION_RETRIEVAL_LIMIT: i64 = 64;
pub const RESOLUTION_PROMPT_LIMIT: usize = 8;

#[derive(Debug, Clone)]
pub struct Subject {
    pub path: String,
    pub names: Vec<String>,
    pub description: String,
    pub category: EntityCategory,
    pub kinds: Vec<String>,
    pub assertions: BTreeSet<String>,
}

impl Subject {
    #[cfg(test)]
    pub fn new(path: &str, name: &str, kinds: &[&str], assertions: &[&str]) -> Self {
        Self {
            path: path.into(),
            names: vec![name.into()],
            description: String::new(),
            category: EntityCategory::Concept,
            kinds: kinds.iter().map(|kind| (*kind).into()).collect(),
            assertions: assertions.iter().map(|value| (*value).into()).collect(),
        }
    }

    pub fn from_parts(
        path: String,
        name: String,
        aliases: impl IntoIterator<Item = String>,
        description: String,
        category: EntityCategory,
        kinds: Vec<String>,
        assertions: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            path,
            names: std::iter::once(name).chain(aliases).collect(),
            description,
            category,
            kinds,
            assertions: assertions.into_iter().collect(),
        }
    }

    pub fn from_entity(entity: &KnowledgeConsolidationEntity) -> Self {
        Self::from_parts(
            entity.path.clone(),
            entity.name.clone(),
            entity.aliases.iter().cloned(),
            entity.description.clone(),
            entity.category,
            entity.kinds.clone(),
            entity.search_assertions.iter().cloned(),
        )
    }

    pub fn name_tokens(&self) -> Vec<String> {
        self.names
            .iter()
            .flat_map(|name| {
                normalize_identity_name(name)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn query_text(&self) -> String {
        std::iter::once(self.names.join(" "))
            .chain((!self.description.trim().is_empty()).then(|| self.description.clone()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub struct Request {
    pub subject: Subject,
    pub eligible_paths: Option<HashSet<String>>,
    pub additional_candidates: Vec<EntityHit>,
    pub forced_paths: Vec<String>,
    pub limit: usize,
}

/// Caller-owned indexed candidate recall. Returned hits are authoritative for
/// lexical retrieval; the phase still applies category/type compatibility,
/// identity ranking, explicit additional candidates, and forced graph identities.
#[async_trait::async_trait]
pub trait CandidateRetrieval: Send + Sync {
    async fn retrieve(&self, subject: &Subject, limit: usize) -> Result<Vec<EntityHit>, Error>;
}

#[derive(Clone)]
pub struct Search {
    repo: GraphSnapshot,
    retrieval: Option<std::sync::Arc<dyn CandidateRetrieval>>,
}

impl Search {
    pub fn new(repo: GraphSnapshot) -> Self {
        Self {
            repo,
            retrieval: None,
        }
    }

    pub fn with_retrieval(
        mut self,
        retrieval: Option<std::sync::Arc<dyn CandidateRetrieval>>,
    ) -> Self {
        self.retrieval = retrieval;
        self
    }

    pub async fn find_candidates<P, F>(
        &self,
        request: Request,
        prepare: P,
        compatibility: F,
    ) -> Result<Vec<RankedCandidate>, Error>
    where
        P: Fn(&mut EntityHit),
        F: Fn(&Subject, &EntityHit) -> Option<u8>,
    {
        let subject = request.subject;
        let query_tokens: HashSet<String> = normalize_identity_name(&subject.query_text())
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        // The host supplies the searchable snapshot. Recall lexical or assertion
        // overlap before ranking, rather than treating every same-category row as
        // a plausible identity. Database-specific full-text retrieval stays outside
        // this crate; a caller can supply an authoritative CandidateRetrieval.
        let mut entities: Vec<EntityHit> = if let Some(retrieval) = &self.retrieval {
            retrieval
                .retrieve(&subject, RESOLUTION_RETRIEVAL_LIMIT as usize)
                .await?
        } else {
            self.repo
                .knowledge_entities()
                .into_iter()
                .filter(|entity| {
                    entity
                        .search_assertions
                        .iter()
                        .any(|a| subject.assertions.contains(a))
                        || std::iter::once(entity.name.as_str())
                            .chain(entity.aliases.iter().map(String::as_str))
                            .chain([entity.description.as_str(), entity.body.as_str()])
                            .any(|text| {
                                normalize_identity_name(text)
                                    .split_whitespace()
                                    .any(|token| query_tokens.contains(token))
                            })
                })
                .map(|entity| EntityHit {
                    path: entity.path,
                    origin: entity.origin,
                    category: entity.category,
                    kinds: entity.kinds,
                    name: entity.name,
                    description: entity.description,
                    aliases: entity.aliases,
                    body: entity.body,
                    search_name_tokens: entity.search_name_tokens,
                    search_assertions: entity.search_assertions,
                })
                .collect()
        };
        let additional_paths: HashSet<_> = request
            .additional_candidates
            .iter()
            .map(|entity| entity.path.as_str())
            .collect();
        entities.retain(|entity| !additional_paths.contains(entity.path.as_str()));
        entities.extend(request.additional_candidates);
        let mut forced = HashSet::new();
        for path in request.forced_paths {
            forced.insert(path.clone());
            if !entities.iter().any(|entity| entity.path == path)
                && let Some(entity) = self.repo.entity_by_path(&path)?
            {
                entities.push(EntityHit {
                    path: entity.path,
                    origin: entity.origin,
                    category: entity.category,
                    kinds: entity.kinds,
                    name: entity.name,
                    description: entity.description,
                    aliases: entity.aliases,
                    body: entity.body,
                    search_name_tokens: entity.search_name_tokens,
                    search_assertions: entity.search_assertions,
                });
            }
        }
        let mut seen = HashSet::new();
        let candidates: Vec<_> = entities
            .into_iter()
            .map(|mut entity| {
                prepare(&mut entity);
                entity
            })
            .filter(|entity| entity.path != subject.path)
            .filter(|entity| entity.category == subject.category)
            .filter(|entity| {
                request
                    .eligible_paths
                    .as_ref()
                    .is_none_or(|paths| paths.contains(&entity.path))
            })
            .filter_map(|entity| {
                compatibility(&subject, &entity).map(|affinity| (entity, affinity))
            })
            .filter(|(entity, _)| seen.insert(entity.path.clone()))
            .map(|(entity, type_affinity)| {
                let assertions = entity.search_assertions.iter().cloned().collect();
                let is_forced = forced.contains(&entity.path);
                RankedCandidate {
                    entity,
                    assertions,
                    evidence: CandidateEvidence {
                        forced: is_forced,
                        type_affinity,
                        ..Default::default()
                    },
                }
            })
            .collect();
        let retrieved = candidates.len();
        let ranked = rank_candidates(&subject, candidates, request.limit);
        tracing::debug!(
            subject_path = %subject.path,
            subject_category = ?subject.category,
            retrieved,
            selected = ranked.len(),
            prompt_limit = request.limit,
            "ranked resolution candidates",
        );
        for (rank, candidate) in ranked.iter().enumerate() {
            tracing::trace!(
                subject_path = %subject.path,
                candidate_path = %candidate.entity.path,
                rank,
                exact_name = candidate.evidence.exact_name,
                token_containment = candidate.evidence.token_containment,
                event_participants_match = candidate.evidence.event_participants_match,
                type_affinity = candidate.evidence.type_affinity,
                shared_assertions = candidate.evidence.shared_assertions.len(),
                similarity = candidate.score(),
                forced = candidate.evidence.forced,
                "resolution candidate evidence",
            );
        }
        Ok(ranked)
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CandidateEvidence {
    pub exact_name: bool,
    pub token_containment: bool,
    pub event_participants_match: bool,
    pub ordered_similarity: f64,
    pub token_order_similarity: f64,
    pub shared_kinds: Vec<String>,
    pub type_affinity: u8,
    pub shared_assertions: Vec<String>,
    pub forced: bool,
}

#[derive(Debug, Clone)]
pub struct RankedCandidate {
    pub entity: EntityHit,
    pub assertions: BTreeSet<String>,
    pub evidence: CandidateEvidence,
}

impl RankedCandidate {
    fn derive_evidence(&mut self, subject: &Subject) {
        let forced = self.evidence.forced;
        let supplied_type_affinity = self.evidence.type_affinity;
        let candidate_names = std::iter::once(self.entity.name.as_str())
            .chain(self.entity.aliases.iter().map(String::as_str))
            .collect::<Vec<_>>();
        let mut exact_name = false;
        let mut token_containment = false;
        let mut event_participants_match = false;
        let mut ordered_similarity = 0.0_f64;
        let mut token_order_similarity = 0.0_f64;
        for ours in &subject.names {
            for theirs in &candidate_names {
                let ours_normalized = normalize_identity_name(ours);
                let theirs_normalized = normalize_identity_name(theirs);
                exact_name |= ours_normalized == theirs_normalized && !ours_normalized.is_empty();
                token_containment |= whole_token_subset(&ours_normalized, &theirs_normalized)
                    || whole_token_subset(&theirs_normalized, &ours_normalized);
                event_participants_match |= event_participants(ours)
                    .zip(event_participants(theirs))
                    .is_some_and(|(ours, theirs)| ours == theirs);
                ordered_similarity = ordered_similarity.max(name_similarity(ours, theirs));
                token_order_similarity = token_order_similarity.max(name_similarity(
                    &token_order_name(ours),
                    &token_order_name(theirs),
                ));
            }
        }
        let candidate_kinds: HashSet<&str> = self.entity.kinds.iter().map(String::as_str).collect();
        let mut shared_kinds: Vec<String> = subject
            .kinds
            .iter()
            .filter(|kind| candidate_kinds.contains(kind.as_str()))
            .cloned()
            .collect();
        shared_kinds.sort();
        shared_kinds.dedup();
        let mut shared_assertions: Vec<String> = subject
            .assertions
            .intersection(&self.assertions)
            .cloned()
            .collect();
        shared_assertions.sort();
        let type_affinity =
            supplied_type_affinity.max(if !shared_kinds.is_empty() { 3 } else { 0 });
        self.evidence = CandidateEvidence {
            exact_name,
            token_containment,
            event_participants_match,
            ordered_similarity,
            token_order_similarity,
            shared_kinds,
            shared_assertions,
            type_affinity,
            forced,
        };
    }

    fn rank(&self) -> (bool, bool, bool, bool, usize, bool, bool, bool) {
        let similar = self.evidence.ordered_similarity >= IDENTITY_SIMILARITY_THRESHOLD
            || self.evidence.token_order_similarity >= IDENTITY_SIMILARITY_THRESHOLD;
        (
            self.evidence.forced,
            self.evidence.exact_name,
            self.evidence.event_participants_match,
            self.evidence.token_containment && self.evidence.type_affinity > 0,
            self.evidence.shared_assertions.len(),
            self.evidence.token_containment,
            self.evidence.type_affinity > 0,
            similar,
        )
    }

    fn score(&self) -> f64 {
        self.evidence
            .ordered_similarity
            .max(self.evidence.token_order_similarity)
    }
}

pub fn rank_candidates(
    subject: &Subject,
    mut candidates: Vec<RankedCandidate>,
    limit: usize,
) -> Vec<RankedCandidate> {
    candidates.retain(|candidate| candidate.entity.path != subject.path);
    for candidate in &mut candidates {
        candidate.derive_evidence(subject);
    }
    candidates.sort_by(|a, b| {
        b.rank()
            .cmp(&a.rank())
            .then_with(|| b.score().total_cmp(&a.score()))
            .then_with(|| a.entity.path.cmp(&b.entity.path))
    });
    let forced = candidates
        .iter()
        .filter(|candidate| candidate.evidence.forced)
        .count();
    candidates.truncate(limit.max(forced));
    candidates
}

fn token_order_name(value: &str) -> String {
    let normalized = normalize_identity_name(value);
    let mut tokens: Vec<&str> = normalized.split_whitespace().collect();
    tokens.sort_unstable();
    tokens.join(" ")
}

fn whole_token_subset(shorter: &str, longer: &str) -> bool {
    let shorter: BTreeSet<&str> = shorter.split_whitespace().collect();
    let longer: BTreeSet<&str> = longer.split_whitespace().collect();
    !shorter.is_empty() && shorter.len() < longer.len() && shorter.is_subset(&longer)
}

fn event_participants(value: &str) -> Option<[String; 2]> {
    let event_name = value
        .split(['—', '–'])
        .next()
        .unwrap_or(value)
        .split(" - ")
        .next()
        .unwrap_or(value);
    let tokens: Vec<&str> = event_name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let separator = tokens.iter().position(|token| {
        token.eq_ignore_ascii_case("vs")
            || token.eq_ignore_ascii_case("v")
            || token.eq_ignore_ascii_case("versus")
    })?;
    if separator == 0 || separator + 1 >= tokens.len() {
        return None;
    }
    let mut participants = [
        normalize_identity_name(&tokens[..separator].join(" ")),
        normalize_identity_name(&tokens[separator + 1..].join(" ")),
    ];
    if participants.iter().any(String::is_empty) {
        return None;
    }
    participants.sort();
    Some(participants)
}

fn name_similarity(a: &str, b: &str) -> f64 {
    let a = normalize_identity_name(a);
    let b = normalize_identity_name(b);
    if a.is_empty() || b.is_empty() {
        0.0
    } else {
        strsim::jaro_winkler(&a, &b)
    }
}

#[cfg(test)]
mod tests;
