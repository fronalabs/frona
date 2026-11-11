use super::*;

fn node(iri: &str) -> oxrdf::NamedOrBlankNode {
    oxrdf::NamedOrBlankNode::NamedNode(oxrdf::NamedNode::new_unchecked(iri))
}

fn edge(subject: &str, property: &str, object: &str) -> Triple {
    Triple::new(
        node(subject),
        oxrdf::NamedNode::new_unchecked(property),
        oxrdf::Term::NamedNode(oxrdf::NamedNode::new_unchecked(object)),
    )
}

fn attribute(subject: &str, property: &str, value: &str) -> Triple {
    Triple::new(
        node(subject),
        oxrdf::NamedNode::new_unchecked(property),
        oxrdf::Term::Literal(oxrdf::Literal::new_simple_literal(value)),
    )
}

#[test]
fn unsupported_adjudication_inverse_is_rejected() {
    let proposal = Proposal {
        term: "frona:plannedUser".into(),
        kind: ProposalKind::ObjectProperty,
        usage_entities: 0,
        usage_links: 1,
        description: "A person who plans to use the subject.".into(),
        proposed_edits: vec![SchemaEdit::DeclareObjectProperty {
            property: "frona:plannedUser".into(),
        }],
    };
    let decision = Decision::Declare {
        parent: None,
        datatype: None,
        domain: None,
        range: None,
        inverse: Some("frona:requiredWorkLanguage".into()),
        characteristics: Vec::new(),
    };
    assert!(validate_declaration_strengthening(
            &decision, &proposal, &[], &[], |term| term.to_string(),
        ).is_err());
    assert!(
        validate_declaration_strengthening(
            &Decision::AcceptProposal,
            &proposal,
            &[],
            &[],
            |term| term.to_string(),
        )
        .is_ok()
    );
}

#[tokio::test]
async fn evidence_backed_strengthening_passes_the_same_projected_abox_validation_as_commit() {
    use crate::ontology::individual_iri;
    let manager = ontology(GraphSnapshot {
        scope_id: "u".into(),
        ..Default::default()
    });
    let px = manager.prefixes();
    let person = individual_iri("people/member");
    let organization = individual_iri("organizations/club");
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let projected = vec![
        edge(&person, rdf_type, &px.expand("schema:Person")),
        edge(&organization, rdf_type, &px.expand("schema:Organization")),
        edge(&person, &px.expand("frona:memberOf"), &organization),
        edge(&organization, &px.expand("frona:hasMember"), &person),
    ];
    let proposal = Proposal {
        term: "frona:hasMember".into(),
        kind: ProposalKind::ObjectProperty,
        usage_entities: 0,
        usage_links: 1,
        description: "An organization has the person identified as the object.".into(),
        proposed_edits: vec![SchemaEdit::DeclareObjectProperty {
            property: "frona:hasMember".into(),
        }],
    };
    let accepted = vec![SchemaEdit::Align {
        frona: "frona:memberOf".into(),
        standard: "schema:memberOf".into(),
        kind: AlignKind::ObjectProperty,
    }];
    let decision = Decision::Declare {
        parent: None,
        datatype: None,
        domain: Some("schema:Organization".into()),
        range: Some("schema:Person".into()),
        inverse: Some("schema:memberOf".into()),
        characteristics: Vec::new(),
    };

    validate_declaration_strengthening(&decision, &proposal, &accepted, &projected, |term| {
        px.expand(term)
    })
    .expect("the two typed, reversed assertions support all three added axioms");

    let mut trial = accepted.clone();
    trial.extend(decision.edits(&proposal.term, proposal.kind));
    let impact = manager
        .test_edits_with_abox("u", &trial, &projected)
        .await
        .unwrap();
    assert!(
        impact.incoherence.is_empty(),
        "schema remains coherent: {impact:?}"
    );
    assert!(
        impact.data_violations.is_empty(),
        "projected data remains valid: {impact:?}"
    );

    let without_reverse: Vec<_> = projected
        .iter()
        .filter(|triple| triple.predicate.as_str() != px.expand("frona:memberOf"))
        .cloned()
        .collect();
    let rejection = validate_declaration_strengthening(
        &decision,
        &proposal,
        &accepted,
        &without_reverse,
        |term| px.expand(term),
    )
    .expect_err("an inverse cannot be inferred from only one direction");
    assert!(
        rejection.contains("InverseProperties"),
        "specific feedback: {rejection}"
    );

    let wrong_range = vec![
        edge(&person, rdf_type, &px.expand("schema:Organization")),
        edge(&organization, rdf_type, &px.expand("schema:Organization")),
        edge(&person, &px.expand("frona:memberOf"), &organization),
        edge(&organization, &px.expand("frona:hasMember"), &person),
    ];
    let rejection =
        validate_declaration_strengthening(&decision, &proposal, &accepted, &wrong_range, |term| {
            px.expand(term)
        })
        .expect_err("the proposed range must match every observed object type");
    assert!(
        rejection.contains("ObjectPropertyRange"),
        "specific feedback: {rejection}"
    );
}

#[tokio::test]
async fn later_batch_cannot_align_a_restricted_property_to_incompatible_existing_data() {
    use crate::ontology::individual_iri;
    let manager = ontology(GraphSnapshot {
        scope_id: "u".into(),
        ..Default::default()
    });
    let px = manager.prefixes();
    let projected = vec![attribute(
        &individual_iri("people/member"),
        &px.expand("schema:age"),
        "unknown",
    )];
    let first_batch = vec![SchemaEdit::RestrictDatatype {
        property: "frona:ageInYears".into(),
        datatype: "xsd:integer".into(),
        min: Some(0),
        max: Some(150),
        pattern: None,
    }];
    let first_impact = manager
        .test_edits_with_abox("u", &first_batch, &projected)
        .await
        .unwrap();
    assert_eq!(gate(&first_impact), GateOutcome::Commit { quarantine: 0 });

    let mut cumulative = first_batch;
    cumulative.push(SchemaEdit::Align {
        frona: "frona:ageInYears".into(),
        standard: "schema:age".into(),
        kind: AlignKind::DataProperty,
    });
    let second_impact = manager
        .test_edits_with_abox(
            "u",
            &cumulative,
            &validation_abox(&projected, &cumulative, &px),
        )
        .await
        .unwrap();
    assert!(
        second_impact.data_violations.iter().any(|violation| {
            violation.subject.as_deref() == Some("people/member")
                && violation.detail.contains("unknown")
        }),
        "the cumulative edit must expose the incompatible existing value: {second_impact:?}",
    );
    assert_eq!(
        gate(&second_impact),
        GateOutcome::DataViolations { affected: 1 },
    );
}

/// An entity is stamped with the term the pass *settled on*, which an align or merge
/// makes different from the term classify proposed.
#[test]
fn renamed_term_is_what_gets_stamped() {
    let mut a = AssemblePlan::default();
    a.note(
        "frona:Company",
        &Decision::Align {
            standard: "schema:Organization".into(),
        },
    );
    a.note(
        "frona:Db",
        &Decision::Merge {
            into: "frona:Database".into(),
        },
    );
    a.note(
        "frona:Database",
        &Decision::Declare {
            parent: Some("schema:SoftwareApplication".into()),
            datatype: None,
            domain: None,
            range: None,
            inverse: None,
            characteristics: Vec::new(),
        },
    );
    a.note("frona:Vague", &Decision::Defer);

    assert_eq!(
        a.final_term("frona:Company"),
        "schema:Organization",
        "aligned"
    );
    assert_eq!(a.final_term("frona:Db"), "frona:Database", "merged");
    assert_eq!(
        a.final_term("frona:Database"),
        "frona:Database",
        "declared → unchanged"
    );
    assert_eq!(
        a.final_term("frona:Vague"),
        "frona:Vague",
        "deferred → still used"
    );
    assert_eq!((a.declared, a.aligned, a.deferred), (1, 2, 1));
}

mod run;

fn ontology(graph: GraphSnapshot) -> Ontology {
    use crate::{
        model::KnowledgeOntology,
        ontology::{OntologyCatalogue, catalogue::roots::Root},
    };
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/assemble-ontology");
    let catalogue = OntologyCatalogue::load(&[(Root::Release, path.as_path())]).unwrap();
    let state = KnowledgeOntology {
        id: "ontology".into(),
        user_id: graph.scope_id.clone(),
        owl: String::new(),
        format: "ofn".into(),
        version: 3,
        effective_ontology: String::new(),
        seeds: vec![],
        sources: vec![],
        catalog_fingerprint: String::new(),
        updated_at: chrono::Utc::now(),
    };
    Ontology::new(catalogue, state, graph)
}

mod refinement;
