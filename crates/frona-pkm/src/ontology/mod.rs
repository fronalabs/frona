//! Snapshot-backed ontology reasoning copied from the server. Catalogue loading is
//! explicit; phases never repair installations, write deltas, or publish caches.
use crate::{
    Error, GraphSnapshot,
    model::{KnowledgeEntity, KnowledgeEntityLink, KnowledgeOntology},
};
use oxigraph::sparql::QueryResults;
use oxrdf::{NamedOrBlankNode, Term, Triple};
use std::sync::Arc;
mod abox;
pub mod catalogue;
mod composition;
mod graph_query;
mod inspection;
mod planning;
mod prefixes;
mod reasoning;
mod retrieval;
pub mod schema;
pub mod sparql;
mod validation;
pub use catalogue::{OntologyCatalogue, OntologyScope, VocabHit};
pub use composition::{ComposedOntology, UserOntology};
pub use graph_query::GraphDirection;
pub use inspection::OntologyExport;
pub use planning::{PlannedSchema, TypePlan};
pub use prefixes::{PrefixMap, TermKind, individual_iri, path_from_individual};
pub use retrieval::{ClassInterpretation, IdentityAmbiguity, SemanticCandidate, SemanticMatch};
pub use schema::{AlignKind, Catalog, Characteristic, OverrideTarget, SchemaEdit};
pub use validation::{
    EditImpact, GraphValidation, ValidationDiagnostic, ValidationDiagnosticKind, Violation,
    ViolationSource,
};
pub const DELTA_FORMAT: &str = "ofn";
const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";

/// Immutable ontology and graph snapshot for one phase. The host supplies the
/// persisted effective cut so definitions from removed sources remain available.
#[derive(Clone)]
pub struct Ontology {
    catalogue: Arc<OntologyCatalogue>,
    state: KnowledgeOntology,
    graph: GraphSnapshot,
}
impl Ontology {
    fn ensure_scope(&self, scope_id: &str) -> Result<(), Error> {
        if scope_id != self.graph.scope_id || scope_id != self.state.user_id {
            return Err(Error::Validation("ontology snapshot scope mismatch".into()));
        }
        Ok(())
    }
    fn asserted_links(&self) -> Vec<KnowledgeEntityLink> {
        self.graph.asserted_links()
    }

    pub fn new(
        catalogue: Arc<OntologyCatalogue>,
        state: KnowledgeOntology,
        graph: GraphSnapshot,
    ) -> Self {
        Self {
            catalogue,
            state,
            graph,
        }
    }
    pub fn prefixes(&self) -> PrefixMap {
        self.catalogue.prefixes().clone()
    }
    pub fn catalogue(&self) -> Option<Arc<OntologyCatalogue>> {
        Some(self.catalogue.clone())
    }
    pub fn graph(&self) -> &GraphSnapshot {
        &self.graph
    }
    pub fn search_vocab(&self, term: &str, limit: usize) -> Vec<VocabHit> {
        self.catalogue.search(term, limit)
    }
    pub async fn user_effective_ontology(
        &self,
        scope_id: &str,
    ) -> Result<Arc<OntologyScope>, Error> {
        Ok(self.load(scope_id).await?.effective_ontology().clone())
    }
    fn referenced_terms(&self) -> Vec<String> {
        let mut terms = Vec::new();
        for entity in self.graph.knowledge_entities() {
            if entity.category == crate::model::EntityCategory::Concept {
                terms.extend([
                    crate::model::ENTITY_NAME_PROPERTY_IRI.to_string(),
                    crate::model::ENTITY_PATH_PROPERTY_IRI.to_string(),
                ]);
            }
            terms.extend(entity.kinds);
            if let Some(attrs) = entity.attributes.as_object() {
                terms.extend(attrs.keys().cloned());
            }
        }
        terms.extend(
            self.graph
                .links
                .iter()
                .filter(|link| link.origin != crate::model::LinkOrigin::Inferred)
                .map(|link| link.relation.clone()),
        );
        terms.sort();
        terms.dedup();
        terms
    }
}

#[cfg(test)]
mod tests;

// Public return types must be nameable by applications composing these phases.
pub use graph_query::{GraphAttribute, GraphEdge, GraphEntity, GraphQueryOutput};
pub use inspection::{
    OntologyPropertyInspection, OntologySearchHit, OntologyTermInspection, OntologyTermRelation,
};
pub use reasoning::{Diagnostic, ReasonPass, Reasoned};
pub use retrieval::{ClassTermInterpretation, SemanticSearchResult};
pub use validation::ValidationDiagnosticGroup;

pub use abox::InferredGraph;
