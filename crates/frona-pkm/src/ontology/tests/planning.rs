use super::*;
fn standard_catalogue() -> Arc<OntologyCatalogue> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ontology/standard");
    OntologyCatalogue::load(&[(Root::Release, &path)]).unwrap()
}
fn iri(curie: &str) -> String {
    PrefixMap::standard().expand(curie)
}
/// The model is told not to return an implied class and is not trusted to comply.
#[test]
fn normalization_keeps_only_the_most_specific_classes() {
    let mgr = standard_catalogue();
    let got = crate::ontology::normalize_types(
        &mgr.clone(),
        &[iri("schema:Person"), iri("schema:Thing")],
        &[],
    );
    assert_eq!(got, [iri("schema:Person")], "Thing is implied by Person");
}

#[test]
fn normalization_canonicalizes_before_deduplication_and_subsumption() {
    let mgr = standard_catalogue();
    let catalogue = mgr.clone();
    let prefixes = catalogue.prefixes();
    assert_eq!(prefixes.expand("schema:Person"), iri("schema:Person"));
    assert_eq!(prefixes.expand("schema:Thing"), iri("schema:Thing"));
    assert_eq!(
        crate::ontology::normalize_types(
            &mgr.clone(),
            &["schema:Person".into(), iri("schema:Person")],
            &[],
        ),
        [iri("schema:Person")],
        "a CURIE and its IRI are one class",
    );
    assert_eq!(
        crate::ontology::normalize_types(
            &mgr.clone(),
            &["schema:Thing".into(), "schema:Person".into()],
            &[],
        ),
        [iri("schema:Person")],
        "the more specific Person type makes Thing redundant",
    );
}

#[test]
fn stamping_an_existing_type_repairs_a_mixed_curie_and_iri_pair() {
    let mgr = standard_catalogue();
    let plan = crate::ontology::plan_entity_type(
        &mgr.clone(),
        &["schema:Person".into(), iri("schema:Person")],
        "schema:Person",
        &[],
    );
    assert_eq!(
        plan,
        TypePlan::Write(vec![iri("schema:Person")]),
        "an already-held type must not bypass canonicalization",
    );
}

/// A class minted *this pass* exists only in the delta, so a catalogue-only check
/// would keep both and defeat the whole exercise.
#[test]
fn normalization_reads_subsumption_out_of_the_delta_too() {
    let mgr = standard_catalogue();
    let engineer = "urn:frona:Engineer".to_string();
    let delta = [Triple::new(
        NamedOrBlankNode::NamedNode(oxrdf::NamedNode::new_unchecked(engineer.clone())),
        oxrdf::NamedNode::new_unchecked(
            "http://www.w3.org/2000/01/rdf-schema#subClassOf".to_string(),
        ),
        Term::NamedNode(oxrdf::NamedNode::new_unchecked(iri("schema:Person"))),
    )];

    assert_eq!(
        crate::ontology::normalize_types(
            &mgr.clone(),
            &[iri("schema:Person"), engineer.clone()],
            &delta
        ),
        std::slice::from_ref(&engineer),
        "the mint is more specific, so Person is implied"
    );
    // Without the delta the relationship is invisible and both must survive -
    // dropping one on a subsumption we cannot see would be a guess.
    assert_eq!(
        crate::ontology::normalize_types(
            &mgr.clone(),
            &[iri("schema:Person"), engineer.clone()],
            &[]
        ),
        [iri("schema:Person"), engineer],
    );
}

#[test]
fn normalization_leaves_unrelated_and_unknown_classes_alone() {
    let mgr = standard_catalogue();
    let unrelated = [iri("schema:Person"), iri("schema:CreativeWork")];
    assert_eq!(
        crate::ontology::normalize_types(&mgr.clone(), &unrelated, &[]),
        unrelated
    );

    // A term no source declares has no known ancestors; keeping it is the only
    // safe answer.
    let unknown = [iri("schema:Person"), "urn:frona:Whatever".to_string()];
    assert_eq!(
        crate::ontology::normalize_types(&mgr.clone(), &unknown, &[]),
        unknown
    );
}

/// Order survives, because "reject the newest on a clash" depends on it.
#[test]
fn normalization_preserves_order_and_deduplicates() {
    let mgr = standard_catalogue();
    let got = crate::ontology::normalize_types(
        &mgr.clone(),
        &[
            iri("schema:CreativeWork"),
            iri("schema:Person"),
            iri("schema:CreativeWork"),
        ],
        &[],
    );
    assert_eq!(got, [iri("schema:CreativeWork"), iri("schema:Person")]);
}
