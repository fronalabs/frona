use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use pulldown_cmark::{Event, Parser};
use serde::Serialize;

use crate::core::error::AppError;
use crate::db::repo::pkm::PkmRepo;

use super::model::{
    EntityCategory, EntityHit, EntityOrigin, RankedEntityHit, normalize_identity_name,
};
use super::ontology::{
    ClassInterpretation, IdentityAmbiguity, OntologyManager, SemanticCandidate, SemanticMatch,
};
use super::vault::VaultScope;

#[derive(Clone)]
pub(crate) struct MemorySearch {
    repo: Arc<PkmRepo>,
    ontology: OntologyManager,
}

impl MemorySearch {
    pub(crate) fn new(repo: Arc<PkmRepo>, ontology: OntologyManager) -> Self {
        Self { repo, ontology }
    }

    pub(crate) async fn execute(
        &self,
        user_id: &str,
        query: &str,
        vault: &VaultScope,
    ) -> Result<MemorySearchOutput, AppError> {
        let result_limit = self.repo.search_top_k();
        let raw_path = query.trim().trim_end_matches(".md");
        let canonical_path = vault
            .page_from_any(query)
            .unwrap_or_else(|| raw_path.to_string());

        let semantic = self
            .ontology
            .search_semantic_entities(user_id, query, Some(&canonical_path), result_limit + 1)
            .await?;
        let mut candidates = BTreeMap::new();
        for candidate in semantic.candidates {
            merge_semantic_candidate(&mut candidates, candidate);
        }

        // Higher-tier paths can also occupy the top of either FTS query. Fetch enough
        // extra rows to replace every such duplicate before the final limit is applied.
        let metadata_limit = result_limit + candidates.len() + 1;
        let metadata = self
            .repo
            .search_entity_metadata_candidates(user_id, query, metadata_limit)
            .await?;
        let metadata_may_have_more = metadata.len() == metadata_limit;
        let metadata_paths: Vec<_> = metadata.iter().map(|hit| hit.entity.path.clone()).collect();
        for hit in metadata {
            merge_ranked_hit(&mut candidates, hit, SearchMatch::MetadataText);
        }

        let body_limit = result_limit + candidates.len() + 1;
        let (metadata_body, body) = tokio::try_join!(
            self.repo
                .search_entity_body_evidence_for_paths(user_id, query, &metadata_paths),
            self.repo
                .search_entity_body_candidates(user_id, query, body_limit),
        )?;
        let body_may_have_more = body.len() == body_limit;
        for hit in metadata_body.into_iter().chain(body) {
            let Some(snippet) = hit.entity.match_snippet(query) else {
                continue;
            };
            merge_ranked_hit(&mut candidates, hit, SearchMatch::BodyText { snippet });
        }

        let mut ranked: Vec<_> = candidates.into_values().collect();
        let query_tokens = text_tokens(query);
        for candidate in &mut ranked {
            candidate.rank = RankSignals::for_candidate(candidate, &query_tokens);
        }
        ranked.sort_by(compare_candidates);
        let truncated = semantic.truncated
            || metadata_may_have_more
            || body_may_have_more
            || ranked.len() > result_limit;
        ranked.truncate(result_limit);

        Ok(MemorySearchOutput {
            query: query.to_string(),
            ontology: semantic.class_interpretation,
            identity_ambiguity: semantic.identity_ambiguity,
            results: ranked
                .into_iter()
                .map(|candidate| candidate.into_output(vault))
                .collect(),
            truncated,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct MemorySearchOutput {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ontology: Option<ClassInterpretation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    identity_ambiguity: Vec<IdentityAmbiguity>,
    results: Vec<MemorySearchResult>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct MemorySearchResult {
    path: String,
    name: String,
    description: String,
    origin: &'static str,
    category: &'static str,
    matched_by: Vec<SearchMatch>,
    file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SearchMatch {
    ExactPath,
    ExactName,
    ExactAlias { value: String },
    AssertedType { term: String },
    InferredType { term: String },
    PathToken { token: String },
    AssertedTypeToken { token: String, term: String },
    InferredTypeToken { token: String, term: String },
    MetadataText,
    BodyText { snippet: String },
}

impl SearchMatch {
    fn evidence_order(&self) -> u8 {
        match self {
            Self::ExactPath => 0,
            Self::ExactName => 1,
            Self::ExactAlias { .. } => 2,
            Self::AssertedType { .. } | Self::InferredType { .. } => 3,
            Self::AssertedTypeToken { .. } | Self::InferredTypeToken { .. } => 4,
            Self::PathToken { .. } => 5,
            Self::MetadataText => 6,
            Self::BodyText { .. } => 7,
        }
    }

    fn stable_values(&self) -> (&str, &str) {
        match self {
            Self::ExactAlias { value } => (value, ""),
            Self::AssertedType { term } | Self::InferredType { term } => (term, ""),
            Self::PathToken { token } => (token, ""),
            Self::AssertedTypeToken { token, term } | Self::InferredTypeToken { token, term } => {
                (token, term)
            }
            Self::BodyText { snippet } => (snippet, ""),
            _ => ("", ""),
        }
    }
}

#[derive(Default)]
struct RankSignals {
    strong_tokens: BTreeSet<String>,
    identity_tokens: BTreeSet<String>,
    class_tokens: BTreeSet<String>,
    path_tokens: BTreeSet<String>,
    description_tokens: BTreeSet<String>,
    visible_body_tokens: BTreeSet<String>,
}

impl RankSignals {
    fn for_candidate(candidate: &Candidate, query_tokens: &BTreeSet<String>) -> Self {
        let mut identity_text = candidate.name.clone();
        for alias in &candidate.aliases {
            identity_text.push(' ');
            identity_text.push_str(alias);
        }
        let identity_tokens = matching_tokens(&identity_text, query_tokens);
        let path_tokens = matching_tokens(&candidate.path, query_tokens);
        let description_tokens = matching_tokens(&candidate.description, query_tokens);
        let visible_body_tokens =
            matching_tokens(&visible_markdown_text(&candidate.body), query_tokens);
        let class_tokens: BTreeSet<String> = candidate
            .matched_by
            .iter()
            .filter_map(|matched| match matched {
                SearchMatch::AssertedTypeToken { token, .. }
                | SearchMatch::InferredTypeToken { token, .. } => Some(token.clone()),
                _ => None,
            })
            .collect();
        let mut strong_tokens = identity_tokens.clone();
        strong_tokens.extend(path_tokens.iter().cloned());
        strong_tokens.extend(class_tokens.iter().cloned());
        strong_tokens.extend(description_tokens.iter().cloned());
        Self {
            strong_tokens,
            identity_tokens,
            class_tokens,
            path_tokens,
            description_tokens,
            visible_body_tokens,
        }
    }
}

struct Candidate {
    path: String,
    origin: EntityOrigin,
    category: EntityCategory,
    name: String,
    description: String,
    aliases: Vec<String>,
    body: String,
    use_count: i64,
    metadata_score: Option<f64>,
    body_score: Option<f64>,
    matched_by: Vec<SearchMatch>,
    rank: RankSignals,
}

impl Candidate {
    fn from_entity_hit(hit: &EntityHit, use_count: i64) -> Self {
        Self {
            path: hit.path.clone(),
            origin: hit.origin,
            category: hit.category,
            name: hit.name.clone(),
            description: hit.description.clone(),
            aliases: hit.aliases.iter().cloned().collect(),
            body: hit.body.clone(),
            use_count,
            metadata_score: None,
            body_score: None,
            matched_by: Vec::new(),
            rank: RankSignals::default(),
        }
    }

    fn from_semantic(candidate: &SemanticCandidate) -> Self {
        Self {
            path: candidate.entity.path.clone(),
            origin: candidate.entity.origin,
            category: candidate.entity.category,
            name: candidate.entity.name.clone(),
            description: candidate.entity.description.clone(),
            aliases: candidate.entity.aliases.iter().cloned().collect(),
            body: candidate.entity.body.clone(),
            use_count: candidate.entity.use_count,
            metadata_score: None,
            body_score: None,
            matched_by: Vec::new(),
            rank: RankSignals::default(),
        }
    }

    fn add_match(&mut self, matched: SearchMatch) {
        if !self.matched_by.contains(&matched) {
            self.matched_by.push(matched);
            self.matched_by.sort_by(|a, b| {
                a.evidence_order()
                    .cmp(&b.evidence_order())
                    .then_with(|| a.stable_values().cmp(&b.stable_values()))
            });
        }
    }

    fn ranking_tier(&self) -> u8 {
        self.matched_by
            .iter()
            .map(|matched| match matched {
                SearchMatch::ExactPath => 0,
                SearchMatch::ExactName => 1,
                SearchMatch::ExactAlias { .. } => 2,
                SearchMatch::AssertedType { .. } | SearchMatch::InferredType { .. } => 3,
                _ => 4,
            })
            .min()
            .unwrap_or(u8::MAX)
    }

    fn into_output(self, vault: &VaultScope) -> MemorySearchResult {
        let file = match self.origin {
            EntityOrigin::External => vault.abs_vault_file(&self.path),
            EntityOrigin::Internal => vault.abs_page_file(&self.path),
        };
        MemorySearchResult {
            path: self.path,
            name: self.name,
            description: self.description,
            origin: match self.origin {
                EntityOrigin::Internal => "internal",
                EntityOrigin::External => "external",
            },
            category: match self.category {
                EntityCategory::Concept => "concept",
                EntityCategory::Playbook => "playbook",
            },
            matched_by: self.matched_by,
            file,
        }
    }
}

fn merge_semantic_candidate(
    candidates: &mut BTreeMap<String, Candidate>,
    semantic: SemanticCandidate,
) {
    let entry = candidates
        .entry(semantic.entity.path.clone())
        .or_insert_with(|| Candidate::from_semantic(&semantic));
    for matched in semantic.matches {
        entry.add_match(match matched {
            SemanticMatch::ExactPath => SearchMatch::ExactPath,
            SemanticMatch::ExactName => SearchMatch::ExactName,
            SemanticMatch::ExactAlias { value } => SearchMatch::ExactAlias { value },
            SemanticMatch::AssertedType { term } => SearchMatch::AssertedType { term },
            SemanticMatch::InferredType { term } => SearchMatch::InferredType { term },
            SemanticMatch::PathToken { token } => SearchMatch::PathToken { token },
            SemanticMatch::AssertedTypeToken { token, term } => {
                SearchMatch::AssertedTypeToken { token, term }
            }
            SemanticMatch::InferredTypeToken { token, term } => {
                SearchMatch::InferredTypeToken { token, term }
            }
        });
    }
}

fn merge_ranked_hit(
    candidates: &mut BTreeMap<String, Candidate>,
    hit: RankedEntityHit,
    matched: SearchMatch,
) {
    let path = hit.entity.path.clone();
    let score = hit.score;
    let entry = candidates
        .entry(path)
        .or_insert_with(|| Candidate::from_entity_hit(&hit.entity, hit.use_count));
    match matched {
        SearchMatch::MetadataText => {
            entry.metadata_score = Some(entry.metadata_score.unwrap_or_default().max(score));
            entry.add_match(SearchMatch::MetadataText);
        }
        SearchMatch::BodyText { snippet } => {
            entry.body_score = Some(entry.body_score.unwrap_or_default().max(score));
            entry.add_match(SearchMatch::BodyText { snippet });
        }
        _ => unreachable!("ranked full-text hits only produce text evidence"),
    }
}

fn compare_candidates(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    let a_tier = a.ranking_tier();
    let b_tier = b.ranking_tier();
    a_tier.cmp(&b_tier).then_with(|| {
        if a_tier < 4 {
            b.use_count
                .cmp(&a.use_count)
                .then_with(|| {
                    if a_tier == 3 {
                        normalize_identity_name(&a.name).cmp(&normalize_identity_name(&b.name))
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| a.path.cmp(&b.path))
        } else {
            b.rank
                .strong_tokens
                .len()
                .cmp(&a.rank.strong_tokens.len())
                .then_with(|| {
                    b.rank
                        .identity_tokens
                        .len()
                        .cmp(&a.rank.identity_tokens.len())
                })
                .then_with(|| b.rank.class_tokens.len().cmp(&a.rank.class_tokens.len()))
                .then_with(|| b.rank.path_tokens.len().cmp(&a.rank.path_tokens.len()))
                .then_with(|| {
                    b.rank
                        .description_tokens
                        .len()
                        .cmp(&a.rank.description_tokens.len())
                })
                .then_with(|| {
                    b.rank
                        .visible_body_tokens
                        .len()
                        .cmp(&a.rank.visible_body_tokens.len())
                })
                .then_with(|| b.use_count.cmp(&a.use_count))
                .then_with(|| {
                    b.metadata_score
                        .unwrap_or_default()
                        .total_cmp(&a.metadata_score.unwrap_or_default())
                })
                .then_with(|| {
                    b.body_score
                        .unwrap_or_default()
                        .total_cmp(&a.body_score.unwrap_or_default())
                })
                .then_with(|| a.path.cmp(&b.path))
        }
    })
}

fn text_tokens(text: &str) -> BTreeSet<String> {
    normalize_identity_name(text)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn matching_tokens(text: &str, query_tokens: &BTreeSet<String>) -> BTreeSet<String> {
    text_tokens(text)
        .intersection(query_tokens)
        .cloned()
        .collect()
}

fn visible_markdown_text(markdown: &str) -> String {
    static WIKILINK: OnceLock<regex::Regex> = OnceLock::new();
    let wikilink = WIKILINK.get_or_init(|| {
        regex::Regex::new(r"\[\[([^\[\]]+)\]\]").expect("wikilink expression is valid")
    });
    let visible_wikilinks = wikilink.replace_all(markdown, |captures: &regex::Captures<'_>| {
        let inner = &captures[1];
        inner
            .split_once('|')
            .map_or(inner, |(_, label)| label)
            .to_string()
    });
    Parser::new(&visible_wikilinks)
        .filter_map(|event| match event {
            Event::Text(text) | Event::Code(text) => Some(text.into_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn hit(path: &str, name: &str, body: &str, score: f64, uses: i64) -> RankedEntityHit {
        RankedEntityHit {
            entity: EntityHit {
                path: path.into(),
                origin: EntityOrigin::Internal,
                category: EntityCategory::Concept,
                kinds: Vec::new(),
                name: name.into(),
                description: String::new(),
                aliases: HashSet::new(),
                search_name_tokens: Vec::new(),
                search_assertions: Vec::new(),
                body: body.into(),
            },
            score,
            use_count: uses,
        }
    }

    fn rank(mut candidates: BTreeMap<String, Candidate>, query: &str) -> Vec<Candidate> {
        let query_tokens = text_tokens(query);
        for candidate in candidates.values_mut() {
            candidate.rank = RankSignals::for_candidate(candidate, &query_tokens);
        }
        let mut ranked: Vec<_> = candidates.into_values().collect();
        ranked.sort_by(compare_candidates);
        ranked
    }

    #[test]
    fn strongest_evidence_ranks_and_duplicate_paths_merge() {
        let mut candidates = BTreeMap::new();
        merge_ranked_hit(
            &mut candidates,
            hit("people/mina", "Mina", "Mina approved it.", 1.0, 0),
            SearchMatch::BodyText {
                snippet: "Mina approved it.".into(),
            },
        );
        merge_ranked_hit(
            &mut candidates,
            hit("people/mina", "Mina", "Mina approved it.", 0.5, 0),
            SearchMatch::MetadataText,
        );
        candidates
            .get_mut("people/mina")
            .unwrap()
            .add_match(SearchMatch::ExactName);
        merge_ranked_hit(
            &mut candidates,
            hit("notes/mina", "Meeting notes", "Mina attended.", 99.0, 100),
            SearchMatch::BodyText {
                snippet: "Mina attended.".into(),
            },
        );

        let ranked = rank(candidates, "Mina");
        assert_eq!(ranked[0].path, "people/mina");
        assert_eq!(ranked[0].matched_by.len(), 3);
        assert_eq!(ranked[1].path, "notes/mina");
    }

    #[test]
    fn metadata_tier_beats_higher_scoring_body_and_usage_is_only_a_tie_break() {
        let mut candidates = BTreeMap::new();
        merge_ranked_hit(
            &mut candidates,
            hit("services/postgres", "Postgres", "", 0.1, 0),
            SearchMatch::MetadataText,
        );
        merge_ranked_hit(
            &mut candidates,
            hit("notes/popular", "Popular", "postgres", 100.0, 10_000),
            SearchMatch::BodyText {
                snippet: "postgres".into(),
            },
        );

        let ranked = rank(candidates, "postgres");
        assert_eq!(ranked[0].path, "services/postgres");
    }

    #[test]
    fn metadata_tier_keeps_the_existing_weighted_body_contribution() {
        let mut candidates = BTreeMap::new();
        merge_ranked_hit(
            &mut candidates,
            hit("notes/metadata-only", "Metadata only", "", 1.0, 0),
            SearchMatch::MetadataText,
        );
        merge_ranked_hit(
            &mut candidates,
            hit("notes/metadata-and-body", "Both", "person", 0.9, 0),
            SearchMatch::MetadataText,
        );
        merge_ranked_hit(
            &mut candidates,
            hit("notes/metadata-and-body", "Both", "person", 1.0, 0),
            SearchMatch::BodyText {
                snippet: "person".into(),
            },
        );

        let ranked = rank(candidates, "person");
        assert_eq!(ranked[0].path, "notes/metadata-and-body");
    }

    #[test]
    fn visible_markdown_uses_link_labels_without_indexing_destinations() {
        let visible = visible_markdown_text(
            "See [[people/me|Mina]], [[projects/orbit]], and [Casey](people/casey).",
        );
        assert_eq!(
            text_tokens(&visible),
            BTreeSet::from([
                "and".into(),
                "casey".into(),
                "mina".into(),
                "orbit".into(),
                "projects".into(),
                "see".into(),
            ])
        );
    }

    #[test]
    fn structural_token_coverage_beats_a_higher_raw_body_score() {
        let mut candidates = BTreeMap::new();
        merge_ranked_hit(
            &mut candidates,
            hit(
                "assistants/dark-matter",
                "Dark Matter",
                "[[people/me|Mina]] chose this assistant.",
                0.49,
                0,
            ),
            SearchMatch::BodyText {
                snippet: "Mina chose this assistant.".into(),
            },
        );
        merge_ranked_hit(
            &mut candidates,
            hit(
                "people/me",
                "Mina",
                "The account owner represented by `people/me`.",
                0.40,
                1,
            ),
            SearchMatch::BodyText {
                snippet: "The account owner represented by `people/me`.".into(),
            },
        );
        let mina = candidates.get_mut("people/me").unwrap();
        mina.add_match(SearchMatch::PathToken {
            token: "people".into(),
        });
        mina.add_match(SearchMatch::AssertedTypeToken {
            token: "persons".into(),
            term: "schema:Person".into(),
        });

        let ranked = rank(candidates, "persons people contacts names");
        assert_eq!(ranked[0].path, "people/me");
    }
}
