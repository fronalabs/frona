use super::*;
use crate::db::repo::pkm::test_support::{seed_asserted_entity_link, seed_reconciled_entity};
use crate::memory::pkm::model::KnowledgeEntityLink;
use crate::memory::pkm::model::{EntityCategory, LinkOrigin};
use frona_pkm::ontology::schema::{Characteristic, OverrideTarget};
use frona_pkm::ontology::sparql;
use frona_pkm::ontology::{TypePlan, ValidationDiagnosticKind, ViolationSource};
use frona_pkm::ontology::{individual_iri, path_from_individual};
use oxigraph::sparql::QueryResults;
use oxrdf::{NamedNode, NamedOrBlankNode, Term, Triple};
use std::path::{Path, PathBuf};
use surrealdb::Surreal;
use surrealdb::engine::local::Mem;

/// The committed standard-vocabulary fixture as the bundled half, and no user half.
/// Not the real catalogue - that ships in the image and is absent from a checkout.
fn roots() -> Roots {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ontology");
    Roots {
        release: base.join("standard"),
        user: base.join("no-user-ontologies"),
    }
}

async fn manager() -> (OntologyManager, Arc<PkmRepo>) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let repo = Arc::new(PkmRepo::new(db, 10));
    (OntologyManager::new(roots(), repo.clone()), repo)
}

/// A manager whose catalogue can be taken away mid-test, so the stored-projection
/// path can be exercised without one.
async fn manager_over(release: &Path) -> (OntologyManager, Arc<PkmRepo>) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let repo = Arc::new(PkmRepo::new(db, 10));
    let roots = Roots {
        release: release.to_path_buf(),
        user: release.join("no-user-ontologies"),
    };
    (OntologyManager::new(roots, repo.clone()), repo)
}

fn iri(curie: &str) -> String {
    PrefixMap::standard().expand(curie)
}

fn type_triple(individual: &str, class_iri: &str) -> Triple {
    Triple::new(
        NamedOrBlankNode::NamedNode(NamedNode::new_unchecked(individual.to_string())),
        NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()),
        Term::NamedNode(NamedNode::new_unchecked(class_iri.to_string())),
    )
}

mod characteristics;
mod composition;
mod graph;
mod inspection;
mod planning;
mod reasoning;

use composition::{seed_concept, stamp};
