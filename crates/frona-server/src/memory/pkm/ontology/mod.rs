//! Ontology Memory - an OWL layer over the PKM.
//!
//! The knowledge base is typed against a **catalogue** - every vocabulary the server
//! can see, pinned artifacts plus whatever the user has dropped in - and a per-user
//! `frona:` delta. Nothing in the catalogue is unconditionally loaded. Each pass cuts
//! a **projection**: the terms the vault actually references, closed upward, ~0.4% of
//! the whole. It turns the user's entities and links into an ABox, reasons over
//! `scope ⊕ delta ⊕ ABox` with the OWL 2 RL reasoner (`reasonable`), materializes the
//! result into an in-memory SPARQL store (`oxigraph`), and reads inferred links +
//! validation diagnostics back.
//!
//! Subsumption and disjointness never take that path. They are graph walks over the
//! catalogue's interned index, which returns exactly what OWL 2 RL derives - a
//! contract asserted in both directions by the `frona-ontologies` repo's CI, not an
//! observation. See [`catalogue`].
//!
//! Modules:
//!   - [`prefixes`]  - CURIE ↔ IRI translation (the storage ↔ reasoner seam)
//!   - [`catalogue`] - everything the server can see, and the
//!     [`OntologyScope`] a pass actually reasons over
//!   - [`sparql`]    - SPARQL execution over a materialized-closure store
//!
//! Identity is always by IRI; prefixes are display shorthand and are never used for
//! lookup. RDF 1.1 §3.2 compares IRIs by simple string comparison and forbids
//! further normalization, so two spellings of "the same" vocabulary are bridged
//! with an `owl:equivalentClass` axiom rather than folded together in code.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use oxigraph::sparql::QueryResults;
use oxrdf::{NamedOrBlankNode, Term, Triple};

use crate::core::error::AppError;
use crate::db::repo::pkm::PkmRepo;
use crate::memory::pkm::model::{KnowledgeEntity, KnowledgeEntityLink};

mod abox;
mod catalogue;
mod prefixes;
mod release;
pub(crate) mod schema;
pub(crate) mod sparql;
mod validation;

mod commit;
mod composition;
mod inspection;
mod lifecycle;
mod planning;
mod reasoning;

pub use catalogue::Roots;
pub(crate) use catalogue::{OntologyCatalogue, OntologyScope, VocabHit};
pub(crate) use composition::{ComposedOntology, UserOntology};
pub use inspection::OntologyExport;
pub(crate) use planning::TypePlan;
pub use prefixes::PrefixMap;
pub(crate) use prefixes::{TermKind, individual_iri, path_from_individual};
pub use schema::{AlignKind, Catalog, Characteristic, OverrideTarget, SchemaEdit};
pub use validation::Violation;
pub(crate) use validation::{EditImpact, GraphValidation, ValidationDiagnostic};
#[cfg(test)]
pub(crate) use validation::{ValidationDiagnosticKind, ViolationSource};

/// The `knowledge_ontology.format` tag for the delta serialization.
pub const DELTA_FORMAT: &str = "ofn";
/// Bound on CAS reload-reapply attempts before giving up (single-writer in
/// practice, so this is only ever hit under a real racing write).
const COMMIT_ATTEMPTS: usize = 8;
const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";

/// Loads and commits per-user ontology deltas against the ontology catalogue.
/// Cheaply cloneable (every field is an `Arc`), so it can live on `AppState`.
///
/// It caches no effective ontology of its own - the catalogue memoises cuts on their seed
/// set, so a pass over an unchanged vault re-derives the same seeds and gets the same
/// `Arc` back. Keeping the cache there rather than here is what makes it invalidate
/// for free: a vault that starts using a new term produces a different key.
#[derive(Clone)]
pub struct OntologyManager {
    /// The catalogue is optional and hot-swappable so it can be repaired or reloaded
    /// after boot. A pass keeps its own `OntologyScope`, so a replacement cannot change
    /// a pass that is already running.
    catalogue: Arc<ArcSwapOption<OntologyCatalogue>>,
    repo: Arc<PkmRepo>,
    /// Shared so every manager clone observes the same invalidations.
    reasoned_graphs: Arc<reasoning::ReasonedGraphCache>,
    /// Where the catalogue is assembled from, so the manager can install or reload one
    /// after boot without making catalogue availability a server-start precondition.
    roots: Roots,
}

#[cfg(test)]
mod tests;
