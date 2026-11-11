use super::*;
use crate::ontology::Violation;
use crate::ontology::ViolationSource;

fn facet_violation(entity: &str) -> Violation {
    Violation {
        source: ViolationSource::Facet,
        rule: "xsd-range".into(),
        detail: format!("frona:port on {entity}: 99999 > max 65535"),
        subject: Some(entity.into()),
    }
}

#[test]
fn incoherence_hard_blocks_regardless_of_cap() {
    let impact = EditImpact {
        incoherence: vec!["cax-dw: Person ⊥ Organization".into()],
        data_violations: vec![],
    };
    assert_eq!(gate(&impact), GateOutcome::Incoherent);
}

#[test]
fn any_data_violation_rejects_the_ontology_edit() {
    let impact = EditImpact {
        incoherence: vec![],
        data_violations: vec![facet_violation("services/a")],
    };
    assert_eq!(gate(&impact), GateOutcome::DataViolations { affected: 1 });
}

#[test]
fn multiple_data_violations_are_rejected() {
    let impact = EditImpact {
        incoherence: vec![],
        data_violations: (0..6)
            .map(|i| facet_violation(&format!("services/{i}")))
            .collect(),
    };
    assert_eq!(gate(&impact), GateOutcome::DataViolations { affected: 6 });
}

fn declare(parent: Option<&str>) -> Decision {
    Decision::Declare {
        parent: parent.map(str::to_string),
        datatype: None,
        domain: None,
        range: None,
        inverse: None,
        characteristics: Vec::new(),
    }
}

/// A class mint is only useful if it hangs off the standard hierarchy, so a
/// declared parent lowers to a subclass axiom rather than a bare declaration.
#[test]
fn declaring_a_class_lowers_to_a_subclass_axiom_when_a_parent_is_given() {
    assert_eq!(
        declare(Some("schema:SoftwareApplication")).edits("frona:Database", ProposalKind::Class),
        vec![SchemaEdit::SubClassOf {
            sub: "frona:Database".into(),
            sup: "schema:SoftwareApplication".into()
        }]
    );
    assert_eq!(
        declare(None).edits("frona:Database", ProposalKind::Class),
        vec![SchemaEdit::DeclareClass {
            class: "frona:Database".into()
        }]
    );
    // the model may send "" for "not applicable" - that is not a parent named "".
    assert_eq!(
        declare(Some("   ")).edits("frona:Database", ProposalKind::Class),
        vec![SchemaEdit::DeclareClass {
            class: "frona:Database".into()
        }]
    );
}

#[test]
fn declaring_an_object_property_carries_domain_range_and_inverse() {
    let d = Decision::Declare {
        parent: None,
        datatype: None,
        domain: Some("schema:Person".into()),
        range: Some("schema:Organization".into()),
        inverse: Some("frona:employs".into()),
        characteristics: Vec::new(),
    };
    let edits = d.edits("frona:worksFor", ProposalKind::ObjectProperty);
    assert_eq!(edits.len(), 4, "{edits:?}");
    assert!(edits.contains(&SchemaEdit::DeclareObjectProperty {
        property: "frona:worksFor".into()
    }));
    assert!(edits.contains(&SchemaEdit::InverseProperties {
        a: "frona:employs".into(),
        b: "frona:worksFor".into()
    }));
}

/// Characteristics ride alongside domain/range/inverse: one axiom each, on top of
/// the declaration, in the order the model gave them.
#[test]
fn declaring_an_object_property_carries_its_characteristics() {
    let d = Decision::Declare {
        parent: None,
        datatype: None,
        domain: None,
        range: None,
        inverse: None,
        characteristics: vec![Characteristic::Transitive, Characteristic::Irreflexive],
    };
    assert_eq!(
        d.edits("frona:partOf", ProposalKind::ObjectProperty),
        vec![
            SchemaEdit::DeclareObjectProperty {
                property: "frona:partOf".into()
            },
            SchemaEdit::PropertyCharacteristic {
                property: "frona:partOf".into(),
                characteristic: Characteristic::Transitive,
            },
            SchemaEdit::PropertyCharacteristic {
                property: "frona:partOf".into(),
                characteristic: Characteristic::Irreflexive,
            },
        ]
    );
}

#[test]
fn repeated_characteristics_collapse_to_one_axiom() {
    let d = Decision::Declare {
        parent: None,
        datatype: None,
        domain: None,
        range: None,
        inverse: None,
        characteristics: vec![
            Characteristic::Symmetric,
            Characteristic::Symmetric,
            Characteristic::Functional,
        ],
    };
    let edits = d.edits("frona:knows", ProposalKind::ObjectProperty);
    assert_eq!(
        edits.len(),
        3,
        "declaration + two distinct characteristics: {edits:?}"
    );
}

/// Characteristics are an object-property notion. A class or data property that
/// carries them anyway (a model filling in every field) declares nothing extra -
/// `owl:TransitiveProperty` on a class is not a thing the reasoner can use.
#[test]
fn characteristics_are_ignored_for_a_class_or_data_property() {
    let d = Decision::Declare {
        parent: None,
        datatype: None,
        domain: None,
        range: None,
        inverse: None,
        characteristics: vec![Characteristic::Transitive],
    };
    assert_eq!(
        d.edits("frona:Database", ProposalKind::Class),
        vec![SchemaEdit::DeclareClass {
            class: "frona:Database".into()
        }]
    );
    assert_eq!(
        d.edits("frona:port", ProposalKind::DataProperty),
        vec![SchemaEdit::DeclareDataProperty {
            property: "frona:port".into()
        }]
    );
}

/// Align is the good outcome: one equivalence axiom, and the term is renamed so
/// entities are stamped with the standard term directly (never mint-then-realign).
#[test]
fn align_lowers_to_an_equivalence_and_renames_the_term() {
    let d = Decision::Align {
        standard: "schema:Organization".into(),
    };
    assert_eq!(
        d.edits("frona:Company", ProposalKind::Class),
        vec![SchemaEdit::Align {
            frona: "frona:Company".into(),
            standard: "schema:Organization".into(),
            kind: AlignKind::Class,
        }]
    );
    assert_eq!(d.rename_target(), Some("schema:Organization"));
}

#[test]
fn merge_renames_without_declaring_anything() {
    let d = Decision::Merge {
        into: "frona:Database".into(),
    };
    assert!(d.edits("frona:Db", ProposalKind::Class).is_empty());
    assert_eq!(d.rename_target(), Some("frona:Database"));
}

/// An amend names the **axiom**, not the proposal it was raised against, so it is
/// independent of both `term` and `kind`: the term the model was inspecting when it
/// noticed the problem is rarely the term the offending axiom is about.
#[test]
fn amend_retracts_the_axiom_it_names_whatever_term_raised_it() {
    let target = OverrideTarget::Characteristic {
        property: "frona:partOf".into(),
        characteristic: Characteristic::Transitive,
    };
    let d = Decision::Amend {
        target: target.clone(),
    };
    let expected = vec![SchemaEdit::AmendOverride { target }];

    for (term, kind) in [
        ("frona:Unrelated", ProposalKind::Class),
        ("frona:alsoUnrelated", ProposalKind::ObjectProperty),
        ("frona:port", ProposalKind::DataProperty),
    ] {
        assert_eq!(
            d.edits(term, kind),
            expected,
            "independent of {term}/{kind:?}"
        );
    }
    assert_eq!(
        d.rename_target(),
        None,
        "nothing is re-keyed by loosening an axiom"
    );
    assert_eq!(d.label(), "amend");
}

/// All three retractable axiom kinds reach `AmendOverride` unchanged - this decision
/// is a pass-through, and the matching logic that decides what actually goes lives in
/// `schema::override_matches`.
#[test]
fn every_override_target_passes_through_to_the_schema_edit() {
    for target in [
        OverrideTarget::Disjoint {
            a: "frona:Tool".into(),
            b: "frona:Service".into(),
        },
        OverrideTarget::Facet {
            property: "frona:port".into(),
        },
        OverrideTarget::Characteristic {
            property: "frona:knows".into(),
            characteristic: Characteristic::Symmetric,
        },
    ] {
        assert_eq!(
            Decision::Amend {
                target: target.clone()
            }
            .edits("frona:X", ProposalKind::Class),
            vec![SchemaEdit::AmendOverride { target }]
        );
    }
}

/// Defer declares nothing and renames nothing - the term stays in use and comes
/// back next pass.
#[test]
fn defer_is_inert() {
    assert!(
        Decision::Defer
            .edits("frona:Whatever", ProposalKind::Class)
            .is_empty()
    );
    assert_eq!(Decision::Defer.rename_target(), None);
}

#[test]
fn restrict_lowers_to_a_datatype_facet() {
    let d = Decision::Restrict {
        datatype: "xsd:integer".into(),
        min: Some(1),
        max: Some(65535),
        pattern: None,
    };
    assert_eq!(
        d.edits("frona:port", ProposalKind::DataProperty),
        vec![SchemaEdit::RestrictDatatype {
            property: "frona:port".into(),
            datatype: "xsd:integer".into(),
            min: Some(1),
            max: Some(65535),
            pattern: None,
        }]
    );
}

#[test]
fn clean_edit_commits_with_no_quarantine() {
    let impact = EditImpact::default();
    assert_eq!(gate(&impact), GateOutcome::Commit { quarantine: 0 });
}

fn proposal(index: usize, parent: Option<&str>) -> Proposal {
    let term = format!("frona:Class{index}");
    let proposed_edits = parent
        .map(|sup| {
            vec![SchemaEdit::SubClassOf {
                sub: term.clone(),
                sup: sup.into(),
            }]
        })
        .unwrap_or_else(|| {
            vec![SchemaEdit::DeclareClass {
                class: term.clone(),
            }]
        });
    Proposal {
        term,
        kind: ProposalKind::Class,
        usage_entities: 1,
        usage_links: 0,
        description: "test class".into(),
        proposed_edits,
    }
}

#[test]
fn fewer_than_minimum_is_a_model_free_final_tail() {
    let proposals: Vec<_> = (0..9).map(|i| proposal(i, Some("schema:Thing"))).collect();
    let partition = partition_proposals(&proposals);
    assert!(partition.batches.is_empty());
    assert_eq!(partition.final_tail.len(), 9);
}

#[test]
fn partition_never_leaves_a_one_term_model_batch() {
    let proposals: Vec<_> = (0..31).map(|i| proposal(i, Some("schema:Thing"))).collect();
    let partition = partition_proposals(&proposals);
    assert!(partition.final_tail.is_empty());
    assert!(
        partition.batches.iter().all(|batch| {
            (ADJUDICATION_BATCH_MIN..=ADJUDICATION_BATCH_MAX).contains(&batch.len())
        })
    );
    assert_eq!(partition.batches.iter().map(Vec::len).sum::<usize>(), 31);
}

#[test]
fn parent_precedes_child_inside_a_hierarchy_partition() {
    let mut proposals: Vec<_> = (0..10).map(|i| proposal(i, Some("schema:Thing"))).collect();
    proposals[1].proposed_edits = vec![SchemaEdit::SubClassOf {
        sub: proposals[1].term.clone(),
        sup: proposals[0].term.clone(),
    }];
    let partition = partition_proposals(&proposals);
    let batch = &partition.batches[0];
    let parent = batch
        .iter()
        .position(|p| p.term == proposals[0].term)
        .unwrap();
    let child = batch
        .iter()
        .position(|p| p.term == proposals[1].term)
        .unwrap();
    assert!(parent < child, "parent must be adjudicated before child");
}
