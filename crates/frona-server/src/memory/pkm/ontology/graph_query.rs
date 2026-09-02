use std::collections::BTreeMap;

use oxigraph::sparql::QueryResults;
use oxrdf::{Literal, NamedNode, Term};
use serde::Serialize;

use crate::core::error::AppError;

use super::prefixes::{KB_NAMESPACE, PrefixMap, individual_iri, path_from_individual};
use super::{OntologyManager, sparql};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";

#[derive(Clone, Copy)]
pub(crate) enum GraphDirection {
    Outgoing,
    Incoming,
    Both,
}

impl GraphDirection {
    fn outgoing(self) -> bool {
        matches!(self, Self::Outgoing | Self::Both)
    }

    fn incoming(self) -> bool {
        matches!(self, Self::Incoming | Self::Both)
    }
}

#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GraphAttribute {
    pub property: String,
    pub value: String,
    pub datatype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GraphEdge {
    pub relation: String,
    pub entity: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct GraphEntity {
    pub path: String,
    pub name: String,
    pub description: String,
    pub types: Vec<String>,
    pub attributes: Vec<GraphAttribute>,
    pub outgoing: Vec<GraphEdge>,
    pub incoming: Vec<GraphEdge>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum GraphQueryOutput {
    Boolean {
        value: bool,
    },
    Solutions {
        columns: Vec<String>,
        rows: Vec<BTreeMap<String, String>>,
        truncated: bool,
    },
}

impl OntologyManager {
    pub(crate) async fn graph_entity(
        &self,
        user_id: &str,
        path: &str,
        direction: GraphDirection,
        relation: Option<&str>,
        edge_limit: usize,
    ) -> Result<Option<GraphEntity>, AppError> {
        let pass = self.cached_reason_user(user_id).await?;
        let prefixes = pass.effective_ontology.prefixes();
        let individual = NamedNode::new(individual_iri(path))
            .map_err(|error| AppError::Validation(format!("invalid entity path: {error}")))?;
        let subject = individual.to_string();
        let relation = relation.map(|value| prefixes.expand(value));
        if let Some(relation) = relation.as_deref() {
            NamedNode::new(relation)
                .map_err(|error| AppError::Validation(format!("invalid relation term: {error}")))?;
        }

        let mut types = Vec::new();
        let mut attributes = Vec::new();
        let mut outgoing = Vec::new();
        let mut incoming = Vec::new();

        let outgoing_query = format!("SELECT ?p ?o WHERE {{ {subject} ?p ?o }}");
        if let QueryResults::Solutions(solutions) =
            sparql::query(&pass.reasoned.store, &outgoing_query, prefixes)?
        {
            for solution in solutions {
                let solution = solution
                    .map_err(|error| AppError::Internal(format!("graph result: {error}")))?;
                let (Some(Term::NamedNode(predicate)), Some(object)) =
                    (solution.get("p"), solution.get("o"))
                else {
                    continue;
                };
                if predicate.as_str() == RDF_TYPE {
                    if let Term::NamedNode(class) = object {
                        types.push(display_iri(prefixes, class.as_str()));
                    }
                    continue;
                }
                if let Term::Literal(literal) = object {
                    attributes.push(attribute(prefixes, predicate.as_str(), literal));
                    continue;
                }
                if !direction.outgoing()
                    || predicate.as_str() == OWL_SAME_AS
                    || relation
                        .as_deref()
                        .is_some_and(|wanted| wanted != predicate.as_str())
                {
                    continue;
                }
                let Term::NamedNode(target) = object else {
                    continue;
                };
                if let Some(path) = path_from_individual(target.as_str()) {
                    outgoing.push(GraphEdge {
                        relation: display_iri(prefixes, predicate.as_str()),
                        entity: path,
                    });
                }
            }
        }

        if direction.incoming() {
            let incoming_query = format!(
                "SELECT ?s ?p WHERE {{ ?s ?p {subject} . \
                 FILTER(STRSTARTS(STR(?s), \"{KB_NAMESPACE}\")) }}"
            );
            if let QueryResults::Solutions(solutions) =
                sparql::query(&pass.reasoned.store, &incoming_query, prefixes)?
            {
                for solution in solutions {
                    let solution = solution
                        .map_err(|error| AppError::Internal(format!("graph result: {error}")))?;
                    let (Some(Term::NamedNode(source)), Some(Term::NamedNode(predicate))) =
                        (solution.get("s"), solution.get("p"))
                    else {
                        continue;
                    };
                    if predicate.as_str() == OWL_SAME_AS
                        || relation
                            .as_deref()
                            .is_some_and(|wanted| wanted != predicate.as_str())
                    {
                        continue;
                    }
                    if let Some(path) = path_from_individual(source.as_str()) {
                        incoming.push(GraphEdge {
                            relation: display_iri(prefixes, predicate.as_str()),
                            entity: path,
                        });
                    }
                }
            }
        }

        types.sort();
        types.dedup();
        attributes.sort();
        attributes.dedup();
        outgoing.sort();
        outgoing.dedup();
        incoming.sort();
        incoming.dedup();
        let truncated = outgoing.len() > edge_limit || incoming.len() > edge_limit;
        outgoing.truncate(edge_limit);
        incoming.truncate(edge_limit);

        if types.is_empty() && attributes.is_empty() && outgoing.is_empty() && incoming.is_empty() {
            return Ok(None);
        }
        Ok(Some(GraphEntity {
            path: path.to_string(),
            name: pass
                .entities
                .iter()
                .find(|entity| entity.path == path)
                .map(|entity| entity.name.clone())
                .unwrap_or_default(),
            description: pass
                .entities
                .iter()
                .find(|entity| entity.path == path)
                .map(|entity| entity.description.clone())
                .unwrap_or_default(),
            types,
            attributes,
            outgoing,
            incoming,
            truncated,
        }))
    }

    pub(crate) async fn query_graph(
        &self,
        user_id: &str,
        query: &str,
        row_limit: usize,
    ) -> Result<GraphQueryOutput, AppError> {
        let pass = self.cached_reason_user(user_id).await?;
        let prefixes = pass.effective_ontology.prefixes();
        match sparql::query(&pass.reasoned.store, query, prefixes)? {
            QueryResults::Boolean(value) => Ok(GraphQueryOutput::Boolean { value }),
            QueryResults::Solutions(solutions) => {
                let columns: Vec<String> = solutions
                    .variables()
                    .iter()
                    .map(|variable| variable.as_str().to_string())
                    .collect();
                let mut rows = Vec::new();
                let mut truncated = false;
                for (index, solution) in solutions.enumerate() {
                    if index >= row_limit {
                        truncated = true;
                        break;
                    }
                    let solution = solution
                        .map_err(|error| AppError::Internal(format!("graph result: {error}")))?;
                    let mut row = BTreeMap::new();
                    for column in &columns {
                        if let Some(term) = solution.get(column.as_str()) {
                            row.insert(column.clone(), display_term(prefixes, term));
                        }
                    }
                    rows.push(row);
                }
                Ok(GraphQueryOutput::Solutions {
                    columns,
                    rows,
                    truncated,
                })
            }
            QueryResults::Graph(_) => Err(AppError::Validation(
                "memory_graph_sparql supports only SELECT and ASK queries".into(),
            )),
        }
    }
}

fn display_iri(prefixes: &PrefixMap, iri: &str) -> String {
    prefixes.compact(iri).unwrap_or_else(|| iri.to_string())
}

fn display_term(prefixes: &PrefixMap, term: &Term) -> String {
    match term {
        Term::NamedNode(node) => path_from_individual(node.as_str())
            .unwrap_or_else(|| display_iri(prefixes, node.as_str())),
        Term::Literal(literal) => literal.value().to_string(),
        Term::BlankNode(node) => format!("_:{}", node.as_str()),
        #[allow(unreachable_patterns)]
        _ => term.to_string(),
    }
}

fn attribute(prefixes: &PrefixMap, property: &str, literal: &Literal) -> GraphAttribute {
    GraphAttribute {
        property: display_iri(prefixes, property),
        value: literal.value().to_string(),
        datatype: display_iri(prefixes, literal.datatype().as_str()),
        language: literal.language().map(str::to_string),
    }
}
