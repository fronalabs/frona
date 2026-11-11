mod snapshot;

use super::*;
use oxigraph::io::RdfFormat;

#[test]
fn inherited_disjointness_is_a_hard_identity_filter() {
    let store = Store::new().unwrap();
    let prefixes = crate::ontology::PrefixMap::standard();
    store
        .load_from_reader(
            RdfFormat::Turtle,
            br#"
        @prefix ex: <urn:ex:> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        ex:Person owl:disjointWith ex:Device .
        ex:Employee rdfs:subClassOf ex:Person .
        ex:Phone rdfs:subClassOf ex:Device .
    "#
            .as_slice(),
        )
        .unwrap();
    assert!(types_provably_disjoint(
        &store,
        &["urn:ex:Employee".into()],
        &["urn:ex:Phone".into()],
        &prefixes
    ));
    assert!(!types_provably_disjoint(
        &store,
        &["urn:ex:Product".into()],
        &["urn:ex:PhysicalDevice".into()],
        &prefixes
    ));
    assert_eq!(
        ontology_type_affinity(
            &store,
            &["urn:ex:Employee".into()],
            &["urn:ex:Person".into()],
            &prefixes
        ),
        Some(2)
    );
    assert_eq!(
        ontology_type_affinity(
            &store,
            &["urn:ex:Employee".into()],
            &["urn:ex:Employee".into()],
            &prefixes
        ),
        Some(3)
    );
    assert_eq!(
        ontology_type_affinity(
            &store,
            &["urn:ex:Employee".into()],
            &["urn:ex:Phone".into()],
            &prefixes
        ),
        None
    );
    assert_eq!(
        ontology_type_affinity(
            &store,
            &["urn:ex:Product".into()],
            &["urn:ex:PhysicalDevice".into()],
            &prefixes
        ),
        Some(0)
    );
}
