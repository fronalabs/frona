use std::collections::{BTreeMap, BTreeSet};

use oxigraph::sparql::QueryResults;
use oxrdf::Term;
use serde::Serialize;

use crate::core::error::AppError;
use crate::memory::pkm::model::{KnowledgeEntity, normalize_identity_name};

use super::inspection::{ExactClassResolution, ResolvedClassTerm};
use super::prefixes::{KB_NAMESPACE, path_from_individual};
use super::{OntologyManager, sparql};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SemanticMatch {
    ExactPath,
    ExactName,
    ExactAlias { value: String },
    AssertedType { term: String },
    InferredType { term: String },
    PathToken { token: String },
    AssertedTypeToken { token: String, term: String },
    InferredTypeToken { token: String, term: String },
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticCandidate {
    pub(crate) entity: KnowledgeEntity,
    pub(crate) matches: Vec<SemanticMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ClassTermInterpretation {
    pub(crate) term: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ClassInterpretation {
    Resolved { terms: Vec<ClassTermInterpretation> },
    Ambiguous { terms: Vec<ClassTermInterpretation> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IdentityAmbiguity {
    pub(crate) path: String,
    pub(crate) name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticSearchResult {
    pub(crate) candidates: Vec<SemanticCandidate>,
    pub(crate) class_interpretation: Option<ClassInterpretation>,
    pub(crate) identity_ambiguity: Vec<IdentityAmbiguity>,
    pub(crate) truncated: bool,
}

impl OntologyManager {
    /// Uses the closure for membership and source rows for asserted versus inferred matches.
    pub(crate) async fn search_semantic_entities(
        &self,
        user_id: &str,
        query: &str,
        canonical_path: Option<&str>,
        class_limit: usize,
    ) -> Result<SemanticSearchResult, AppError> {
        let pass = self.cached_reason_user(user_id).await?;
        let normalized_query = normalize_identity_name(query);
        let query_tokens: BTreeSet<_> = normalized_query
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let mut candidates: BTreeMap<String, SemanticCandidate> = BTreeMap::new();
        let mut identity_paths = BTreeSet::new();

        for entity in &pass.entities {
            let mut matches = Vec::new();
            if canonical_path.is_some_and(|path| path == entity.path) {
                matches.push(SemanticMatch::ExactPath);
            }
            if !normalized_query.is_empty()
                && normalize_identity_name(&entity.name) == normalized_query
            {
                matches.push(SemanticMatch::ExactName);
            }
            let mut aliases: Vec<_> = entity
                .aliases
                .iter()
                .filter(|alias| normalize_identity_name(alias) == normalized_query)
                .cloned()
                .collect();
            aliases.sort();
            matches.extend(
                aliases
                    .into_iter()
                    .map(|value| SemanticMatch::ExactAlias { value }),
            );
            if matches.is_empty() {
                continue;
            }
            identity_paths.insert(entity.path.clone());
            candidates.insert(
                entity.path.clone(),
                SemanticCandidate {
                    entity: entity.clone(),
                    matches,
                },
            );
        }

        let has_exact_path = candidates.values().any(|candidate| {
            candidate
                .matches
                .iter()
                .any(|matched| matches!(matched, SemanticMatch::ExactPath))
        });
        let identity_ambiguity = if !has_exact_path && identity_paths.len() > 1 {
            identity_paths
                .iter()
                .filter_map(|path| {
                    candidates.get(path).map(|candidate| IdentityAmbiguity {
                        path: path.clone(),
                        name: candidate.entity.name.clone(),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        for entity in &pass.entities {
            let path_tokens: BTreeSet<_> = normalize_identity_name(&entity.path)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            let matches: Vec<_> = query_tokens
                .intersection(&path_tokens)
                .map(|token| SemanticMatch::PathToken {
                    token: token.clone(),
                })
                .collect();
            if matches.is_empty() {
                continue;
            }
            candidates
                .entry(entity.path.clone())
                .and_modify(|candidate| candidate.matches.extend(matches.clone()))
                .or_insert_with(|| SemanticCandidate {
                    entity: entity.clone(),
                    matches,
                });
        }

        let resolution = self.resolve_published_class_query(&pass, query)?;
        let mut class_interpretation = None;
        let mut class_truncated = false;
        if let ExactClassResolution::Ambiguous(terms) = &resolution {
            class_interpretation = Some(ClassInterpretation::Ambiguous {
                terms: interpretation_terms(terms),
            });
        }
        if let ExactClassResolution::Resolved(terms) = &resolution {
            class_interpretation = Some(ClassInterpretation::Resolved {
                terms: interpretation_terms(terms),
            });
            let mut class_entities = class_entities(&pass, terms)?;
            class_entities.sort_by(|a, b| {
                b.use_count
                    .cmp(&a.use_count)
                    .then_with(|| {
                        normalize_identity_name(&a.name).cmp(&normalize_identity_name(&b.name))
                    })
                    .then_with(|| a.path.cmp(&b.path))
            });
            class_entities.dedup_by(|a, b| a.path == b.path);
            class_truncated = class_entities.len() > class_limit;
            class_entities.truncate(class_limit);

            for entity in class_entities {
                let matched = terms
                    .iter()
                    .find(|term| entity.kinds.iter().any(|kind| kind == &term.iri))
                    .map(|term| SemanticMatch::AssertedType {
                        term: term.term.clone(),
                    })
                    .or_else(|| {
                        terms.first().map(|term| SemanticMatch::InferredType {
                            term: term.term.clone(),
                        })
                    });
                let Some(matched) = matched else { continue };
                candidates
                    .entry(entity.path.clone())
                    .and_modify(|candidate| candidate.matches.push(matched.clone()))
                    .or_insert_with(|| SemanticCandidate {
                        entity,
                        matches: vec![matched],
                    });
            }
        }

        if query_tokens.len() > 1 {
            for token in &query_tokens {
                let ExactClassResolution::Resolved(terms) =
                    self.resolve_published_class_query(&pass, token)?
                else {
                    continue;
                };
                for entity in class_entities(&pass, &terms)? {
                    let matched = terms
                        .iter()
                        .find(|term| entity.kinds.iter().any(|kind| kind == &term.iri))
                        .map(|term| SemanticMatch::AssertedTypeToken {
                            token: token.clone(),
                            term: term.term.clone(),
                        })
                        .or_else(|| {
                            terms.first().map(|term| SemanticMatch::InferredTypeToken {
                                token: token.clone(),
                                term: term.term.clone(),
                            })
                        });
                    let Some(matched) = matched else { continue };
                    candidates
                        .entry(entity.path.clone())
                        .and_modify(|candidate| candidate.matches.push(matched.clone()))
                        .or_insert_with(|| SemanticCandidate {
                            entity,
                            matches: vec![matched],
                        });
                }
            }
        }

        Ok(SemanticSearchResult {
            candidates: candidates.into_values().collect(),
            class_interpretation,
            identity_ambiguity,
            truncated: class_truncated,
        })
    }
}

fn class_entities(
    pass: &super::reasoning::ReasonPass,
    terms: &[ResolvedClassTerm],
) -> Result<Vec<KnowledgeEntity>, AppError> {
    let values = terms
        .iter()
        .map(|term| format!("<{}>", term.iri))
        .collect::<Vec<_>>()
        .join(" ");
    let query = format!(
        "SELECT DISTINCT ?entity WHERE {{ VALUES ?class {{ {values} }} \
         ?entity a ?class . \
         FILTER(STRSTARTS(STR(?entity), \"{KB_NAMESPACE}\")) }}"
    );
    let mut entities = Vec::new();
    if let QueryResults::Solutions(solutions) = sparql::query(
        &pass.reasoned.store,
        &query,
        pass.effective_ontology.prefixes(),
    )? {
        for solution in solutions {
            let solution =
                solution.map_err(|error| AppError::Internal(format!("graph result: {error}")))?;
            let Some(Term::NamedNode(entity)) = solution.get("entity") else {
                continue;
            };
            let Some(path) = path_from_individual(entity.as_str()) else {
                continue;
            };
            let Some(source) = pass.entities.iter().find(|item| item.path == path) else {
                continue;
            };
            entities.push(source.clone());
        }
    }
    entities.sort_by(|a, b| a.path.cmp(&b.path));
    entities.dedup_by(|a, b| a.path == b.path);
    Ok(entities)
}

fn interpretation_terms(terms: &[ResolvedClassTerm]) -> Vec<ClassTermInterpretation> {
    terms
        .iter()
        .map(|term| ClassTermInterpretation {
            term: term.term.clone(),
            label: term.label.clone(),
        })
        .collect()
}
