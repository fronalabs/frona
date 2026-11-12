use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oxrdf::{NamedOrBlankNode, Term};

use crate::core::error::AppError;
use crate::db::repo::pkm::PkmRepo;

use super::model::{
    EntityHit, KnowledgeEntity, KnowledgeEntityLink, KnowledgeEntitySource, KnowledgeMemory,
};
use super::ontology::OntologyManager;
use frona_pkm::ontology::UserOntology;

const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

#[derive(Clone, Default)]
pub struct OntologyRead {
    labels: HashMap<String, String>,
    parents: HashMap<String, Vec<String>>,
    ranges: HashMap<String, String>,
}

#[derive(Clone)]
pub struct OntologyAncestor {
    pub iri: String,
    pub label: String,
}

impl OntologyRead {
    fn from_user(ontology: &UserOntology) -> Self {
        let mut read = Self::default();
        for triple in ontology
            .effective_ontology()
            .triples()
            .iter()
            .chain(ontology.delta_triples())
        {
            let NamedOrBlankNode::NamedNode(subject) = &triple.subject else {
                continue;
            };
            match triple.predicate.as_str() {
                RDFS_LABEL => {
                    if let Term::Literal(label) = &triple.object {
                        read.labels
                            .entry(subject.as_str().into())
                            .or_insert_with(|| label.value().into());
                    }
                }
                RDFS_SUBCLASS_OF => {
                    if let Term::NamedNode(parent) = &triple.object {
                        read.parents
                            .entry(subject.as_str().into())
                            .or_default()
                            .push(parent.as_str().into());
                    }
                }
                RDFS_RANGE => {
                    if let Term::NamedNode(range) = &triple.object {
                        read.ranges
                            .entry(subject.as_str().into())
                            .or_insert_with(|| range.as_str().into());
                    }
                }
                _ => {}
            }
        }
        read
    }

    pub fn label(&self, term: &str) -> String {
        self.labels
            .get(term)
            .cloned()
            .unwrap_or_else(|| display_term(term))
    }

    pub fn datatype(&self, property: &str) -> Option<String> {
        self.ranges.get(property).map(|range| self.label(range))
    }

    pub fn ancestors(&self, iri: &str) -> Vec<OntologyAncestor> {
        let mut visited = HashSet::new();
        let mut frontier = vec![iri.to_string()];
        let mut ancestors = Vec::new();
        while let Some(child) = frontier.pop() {
            for parent in self.parents.get(&child).into_iter().flatten() {
                if visited.insert(parent.clone()) {
                    ancestors.push(OntologyAncestor {
                        iri: parent.clone(),
                        label: self.label(parent),
                    });
                    frontier.push(parent.clone());
                }
            }
        }
        ancestors
    }

    pub fn top_branch(&self, iri: &str) -> String {
        self.ancestors(iri)
            .last()
            .map(|ancestor| ancestor.iri.clone())
            .unwrap_or_else(|| iri.into())
    }
}

fn display_term(term: &str) -> String {
    let local = term.rsplit(['#', '/', ':']).next().unwrap_or(term);
    let mut label = String::new();
    for (index, character) in local.chars().enumerate() {
        if index > 0 && character.is_uppercase() {
            label.push(' ');
        }
        label.push(character);
    }
    label
}

#[derive(Clone)]
pub struct PkmReadService {
    repo: Arc<PkmRepo>,
    ontology: OntologyManager,
}

pub struct PkmGraphRead {
    pub entities: Vec<KnowledgeEntity>,
    pub links: Vec<KnowledgeEntityLink>,
    pub sources: Vec<KnowledgeEntitySource>,
    pub ontology: OntologyRead,
}

pub struct PkmEntityRead {
    pub entity: KnowledgeEntity,
    pub entities: Vec<KnowledgeEntity>,
    pub links: Vec<KnowledgeEntityLink>,
    pub memories: Vec<KnowledgeMemory>,
    pub ontology: OntologyRead,
}

impl PkmReadService {
    pub fn new(repo: Arc<PkmRepo>, ontology: OntologyManager) -> Self {
        Self { repo, ontology }
    }

    pub async fn graph(&self, user_id: &str) -> Result<PkmGraphRead, AppError> {
        Ok(PkmGraphRead {
            entities: self.repo.list_entities(user_id).await?,
            links: self.repo.list_entity_links(user_id).await?,
            sources: self.repo.list_entity_sources(user_id).await?,
            ontology: self.ontology(user_id).await?,
        })
    }

    pub async fn entity(&self, user_id: &str, path: &str) -> Result<PkmEntityRead, AppError> {
        let entity = self
            .repo
            .entity_by_path(user_id, path)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("memory entity not found: {path}")))?;
        Ok(PkmEntityRead {
            entity,
            entities: self.repo.list_entities(user_id).await?,
            links: self.repo.list_entity_links(user_id).await?,
            memories: self.repo.memories_for_entity(user_id, path).await?,
            ontology: self.ontology(user_id).await?,
        })
    }

    async fn ontology(&self, user_id: &str) -> Result<OntologyRead, AppError> {
        if !self.ontology.is_ready() {
            return Ok(OntologyRead::default());
        }
        Ok(OntologyRead::from_user(&self.ontology.load(user_id).await?))
    }

    pub async fn search(&self, user_id: &str, query: &str) -> Result<Vec<EntityHit>, AppError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        self.repo.search_entities(user_id, query).await
    }
}
