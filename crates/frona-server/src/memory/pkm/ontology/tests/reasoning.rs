#[tokio::test]
async fn delta_extends_base_and_reasons_across_boundary() {
    let (mgr, _repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::SubClassOf {
            sub: "frona:Database".into(),
            sup: "schema:SoftwareApplication".into(),
        }],
    )
    .await
    .unwrap();
    let u = mgr.load("u").await.unwrap();
    assert_eq!(u.version(), 1);

    let pg = individual_iri("services/pg");
    let abox = vec![type_triple(&pg, "urn:frona:Database")];
    let reasoned = u.reason(&abox).unwrap();

    for cls in ["schema:CreativeWork", "schema:Thing"] {
        let q = format!("ASK {{ <{pg}> a {cls} }}");
        assert!(
            sparql::ask(&reasoned.store, &q, u.prefixes()).unwrap(),
            "pg inferred {cls}"
        );
    }
    assert_eq!(
        reasoned.clashes().count(),
        0,
        "well-typed data has no clashes"
    );
}

#[tokio::test]
async fn commit_bumps_version_and_cas_rejects_stale() {
    let (mgr, _repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::DeclareClass {
            class: "frona:Service".into(),
        }],
    )
    .await
    .unwrap();
    let u = mgr.load("u").await.unwrap();
    assert_eq!(u.version(), 1);

    let miss = mgr
        .try_commit(
            "u",
            &[SchemaEdit::DeclareClass {
                class: "frona:Team".into(),
            }],
            0,
        )
        .await
        .unwrap();
    assert!(miss.is_none(), "stale expected_version → CAS miss");

    mgr.commit(
        "u",
        &[SchemaEdit::DeclareClass {
            class: "frona:Team".into(),
        }],
    )
    .await
    .unwrap();
    let u = mgr.load("u").await.unwrap();
    assert_eq!(u.version(), 2);
    let cat = u.catalog().unwrap();
    assert!(cat.classes.contains(&"frona:Service".to_string()));
    assert!(cat.classes.contains(&"frona:Team".to_string()));
}

#[tokio::test]
async fn usage_impact_counts_entities_and_links() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "svc/a", "frona:Service").await;
    seed_concept(&repo, "u", "svc/b", "frona:Service").await;
    seed_asserted_entity_link(&repo, "u", "svc/a", "svc/b", "frona:dependsOn")
        .await
        .unwrap();

    let (entities, links) = mgr.usage_impact("u", "frona:Service").await.unwrap();
    assert_eq!((entities, links), (2, 0));
    let (_, dep_links) = mgr.usage_impact("u", "frona:dependsOn").await.unwrap();
    assert_eq!(dep_links, 1);
}

#[tokio::test]
async fn test_edits_distinguishes_consistent_and_incoherent_schema() {
    let (mgr, _repo) = manager().await;
    let ok = mgr
        .test_edits(
            "u",
            &[SchemaEdit::SubClassOf {
                sub: "frona:Service".into(),
                sup: "schema:SoftwareApplication".into(),
            }],
        )
        .await
        .unwrap();
    assert!(ok.incoherence.is_empty(), "consistent edit: {ok:?}");

    let bad = mgr
        .test_edits(
            "u",
            &[
                SchemaEdit::SubClassOf {
                    sub: "frona:Weird".into(),
                    sup: "schema:Person".into(),
                },
                SchemaEdit::SubClassOf {
                    sub: "frona:Weird".into(),
                    sup: "schema:Organization".into(),
                },
            ],
        )
        .await
        .unwrap();
    assert!(
        bad.incoherence.iter().any(|c| c.contains("cax-dw")),
        "unsatisfiable class caught: {bad:?}"
    );
}

#[tokio::test]
async fn edit_validation_sees_pending_edges_and_types_in_a_projected_abox() {
    let (mgr, _repo) = manager().await;
    let px = mgr.prefixes();
    let person = individual_iri("people/me");
    let assistant = individual_iri("ai/example-assistant");
    let mut projected = vec![
        type_triple(&person, &px.expand("schema:Person")),
        type_triple(&assistant, &px.expand("schema:Organization")),
    ];
    projected.push(Triple::new(
        NamedOrBlankNode::NamedNode(oxrdf::NamedNode::new_unchecked(person)),
        oxrdf::NamedNode::new_unchecked(px.expand("frona:hasAssistant")),
        Term::NamedNode(oxrdf::NamedNode::new_unchecked(assistant)),
    ));
    let edits = [
        SchemaEdit::DeclareObjectProperty {
            property: "frona:hasAssistant".into(),
        },
        SchemaEdit::ObjectPropertyRange {
            property: "frona:hasAssistant".into(),
            class: "schema:Person".into(),
        },
    ];

    let without_projection = mgr.test_edits("u", &edits).await.unwrap();
    assert!(
        without_projection.data_violations.is_empty(),
        "the committed graph has no edge yet, so it cannot expose the bad range"
    );
    let with_projection = mgr
        .test_edits_with_abox("u", &edits, &projected)
        .await
        .unwrap();
    assert!(
        !with_projection.data_violations.is_empty(),
        "the future edge and target type must participate in schema validation"
    );
}

#[tokio::test]
async fn sparql_answers_employees_of_org() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::DeclareObjectProperty {
            property: "frona:worksFor".into(),
        }],
    )
    .await
    .unwrap();
    seed_concept(&repo, "u", "people/sarah", "schema:Person").await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    seed_asserted_entity_link(&repo, "u", "people/sarah", "orgs/acme", "frona:worksFor")
        .await
        .unwrap();
    seed_asserted_entity_link(&repo, "u", "people/bob", "orgs/acme", "frona:worksFor")
        .await
        .unwrap();

    let q = format!(
        "SELECT ?p WHERE {{ ?p frona:worksFor <{}> }}",
        individual_iri("orgs/acme")
    );
    let mut people = Vec::new();
    if let QueryResults::Solutions(sols) = mgr.sparql("u", &q).await.unwrap() {
        for s in sols {
            if let Some(Term::NamedNode(n)) = s.unwrap().get("p") {
                people.push(path_from_individual(n.as_str()).unwrap());
            }
        }
    }
    people.sort();
    assert_eq!(
        people,
        vec!["people/bob".to_string(), "people/sarah".to_string()]
    );
}

#[tokio::test]
async fn graph_queries_share_cache_until_consolidation_publishes() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;

    let alice = individual_iri("people/alice");
    let query = format!("ASK {{ <{alice}> a schema:Thing }}");
    assert!(
        matches!(
            mgr.sparql("u", &query).await.unwrap(),
            QueryResults::Boolean(false)
        ),
        "the first query materializes and caches the committed graph"
    );

    seed_concept(&repo, "u", "people/alice", "schema:Person").await;
    assert!(
        matches!(
            mgr.sparql("u", &query).await.unwrap(),
            QueryResults::Boolean(false)
        ),
        "a direct database change is not visible before consolidation publishes"
    );

    mgr.clone().publish_consolidated_graph("u");
    assert!(
        matches!(
            mgr.sparql("u", &query).await.unwrap(),
            QueryResults::Boolean(true)
        ),
        "manager clones share invalidation and the next query rebuilds"
    );
}

#[tokio::test]
async fn ontology_commit_waits_for_consolidation_publication() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::DeclareClass {
            class: "frona:Database".into(),
        }],
    )
    .await
    .unwrap();
    seed_concept(&repo, "u", "services/pg", "frona:Database").await;

    let pg = individual_iri("services/pg");
    let query = format!("ASK {{ <{pg}> a schema:SoftwareApplication }}");
    assert!(matches!(
        mgr.sparql("u", &query).await.unwrap(),
        QueryResults::Boolean(false)
    ));

    mgr.commit(
        "u",
        &[SchemaEdit::SubClassOf {
            sub: "frona:Database".into(),
            sup: "schema:SoftwareApplication".into(),
        }],
    )
    .await
    .unwrap();
    assert!(matches!(
        mgr.sparql("u", &query).await.unwrap(),
        QueryResults::Boolean(false)
    ));

    mgr.clone().publish_consolidated_graph("u");
    assert!(
        matches!(
            mgr.sparql("u", &query).await.unwrap(),
            QueryResults::Boolean(true)
        ),
        "the committed TBox becomes visible at the consolidation publication boundary"
    );
}

#[tokio::test]
async fn abox_entity_is_queryable_through_the_base() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::SubClassOf {
            sub: "frona:Database".into(),
            sup: "schema:SoftwareApplication".into(),
        }],
    )
    .await
    .unwrap();
    seed_concept(&repo, "u", "services/pg", "frona:Database").await;

    mgr.materialize("u").await.unwrap();
    let pg = individual_iri("services/pg");
    let q = format!("ASK {{ <{pg}> a schema:CreativeWork }}");
    let pass = mgr.reason_user("u").await.unwrap();
    assert!(sparql::ask(&pass.reasoned.store, &q, pass.effective_ontology.prefixes()).unwrap());
}

#[tokio::test]
async fn disjoint_types_raise_a_clash_violation() {
    let (mgr, repo) = manager().await;
    mgr.commit(
        "u",
        &[
            SchemaEdit::SubClassOf {
                sub: "frona:Weird".into(),
                sup: "schema:Person".into(),
            },
            SchemaEdit::SubClassOf {
                sub: "frona:Weird".into(),
                sup: "schema:Organization".into(),
            },
        ],
    )
    .await
    .unwrap();
    seed_concept(&repo, "u", "things/w", "frona:Weird").await;

    let violations = mgr.materialize("u").await.unwrap();
    let clash = violations
        .iter()
        .find(|v| v.source == ViolationSource::Reasoner && v.rule == "cax-dw")
        .expect("disjointness clash reported");
    assert_eq!(clash.subject.as_deref(), Some("things/w"));
}

/// The `[1,65535]` bound used to come from `frona.ttl`. The catalogue's interned
/// index does not model `owl:withRestrictions` - the extractor keeps taxonomy,
/// disjointness and equivalence, which is what makes its walk exact - so a facet
/// now reaches the reasoner only through a delta. Declared here rather than
/// bundled; everything downstream of the declaration is unchanged.
async fn declare_port_facet(mgr: &OntologyManager) {
    mgr.commit(
        "u",
        &[SchemaEdit::RestrictDatatype {
            property: "frona:port".into(),
            datatype: "xsd:integer".into(),
            min: Some(1),
            max: Some(65535),
            pattern: None,
        }],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn facet_bound_flags_out_of_range_port() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "services/pg", "frona:Service").await;
    declare_port_facet(&mgr).await;

    seed_reconciled_entity(
        &repo,
        "u",
        "services/pg",
        "",
        "db",
        &serde_json::json!({"frona:port": 99999}),
    )
    .await
    .unwrap();
    let violations = mgr.materialize("u").await.unwrap();
    assert!(
        violations
            .iter()
            .any(|v| v.source == ViolationSource::Facet && v.detail.contains("port")),
        "99999 is out of [1,65535]: {violations:?}"
    );

    seed_reconciled_entity(
        &repo,
        "u",
        "services/pg",
        "",
        "db",
        &serde_json::json!({"frona:port": 5432}),
    )
    .await
    .unwrap();
    let violations = mgr.materialize("u").await.unwrap();
    assert!(
        !violations
            .iter()
            .any(|v| v.source == ViolationSource::Facet),
        "5432 is valid: {violations:?}"
    );
}

#[tokio::test]
async fn facet_bound_inherited_via_subproperty() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "services/pg", "frona:Service").await;
    declare_port_facet(&mgr).await;
    // frona:httpPort ⊑ frona:port. A value asserted under the sub-property is
    // materialized onto frona:port in the closure, so it must inherit the
    // [1,65535] bound even though the facet is declared only on frona:port.
    mgr.commit(
        "u",
        &[
            SchemaEdit::DeclareDataProperty {
                property: "frona:httpPort".into(),
            },
            SchemaEdit::SubPropertyOf {
                sub: "frona:httpPort".into(),
                sup: "frona:port".into(),
            },
        ],
    )
    .await
    .unwrap();
    seed_reconciled_entity(
        &repo,
        "u",
        "services/pg",
        "",
        "db",
        &serde_json::json!({"frona:httpPort": 99999}),
    )
    .await
    .unwrap();
    let violations = mgr.materialize("u").await.unwrap();
    assert!(
        violations
            .iter()
            .any(|v| v.source == ViolationSource::Facet && v.detail.contains("port")),
        "a value under the sub-property inherits the frona:port bound: {violations:?}"
    );
}

#[tokio::test]
async fn restrict_datatype_edit_mints_and_enforces_facet() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "services/pg", "frona:Service").await;
    // The Classify stage mints a PER-USER facet into the delta - not in the base.
    mgr.commit(
        "u",
        &[SchemaEdit::RestrictDatatype {
            property: "frona:replicaCount".into(),
            datatype: "xsd:integer".into(),
            min: Some(1),
            max: None,
            pattern: None,
        }],
    )
    .await
    .unwrap();

    // A value that violates the DELTA-declared bound (0 < min 1). The facet is
    // read out of the closure (from the delta, not the base) and enforced.
    seed_reconciled_entity(
        &repo,
        "u",
        "services/pg",
        "",
        "db",
        &serde_json::json!({"frona:replicaCount": 0}),
    )
    .await
    .unwrap();
    let violations = mgr.materialize("u").await.unwrap();
    assert!(
        violations
            .iter()
            .any(|v| v.source == ViolationSource::Facet && v.detail.contains("replicaCount")),
        "delta-minted facet extracted from the closure + enforced: {violations:?}"
    );

    seed_reconciled_entity(
        &repo,
        "u",
        "services/pg",
        "",
        "db",
        &serde_json::json!({"frona:replicaCount": 3}),
    )
    .await
    .unwrap();
    let ok = mgr.materialize("u").await.unwrap();
    assert!(
        !ok.iter().any(|v| v.source == ViolationSource::Facet),
        "3 ≥ 1 is valid: {ok:?}"
    );
}

#[tokio::test]
async fn ontology_edit_reports_data_violation_over_real_abox() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "services/pg", "frona:Service").await;
    // Existing data predates the constraint: replicaCount 0.
    seed_reconciled_entity(
        &repo,
        "u",
        "services/pg",
        "",
        "db",
        &serde_json::json!({"frona:replicaCount": 0}),
    )
    .await
    .unwrap();

    // Dry-run a min-1 bound: the schema stays coherent, but one existing entity
    // would be flagged - the facet-aware test_edit surfaces it over the real ABox.
    let impact = mgr
        .test_edits(
            "u",
            &[SchemaEdit::RestrictDatatype {
                property: "frona:replicaCount".into(),
                datatype: "xsd:integer".into(),
                min: Some(1),
                max: None,
                pattern: None,
            }],
        )
        .await
        .unwrap();
    assert!(
        impact.incoherence.is_empty(),
        "schema stays coherent: {impact:?}"
    );
    assert!(
        impact
            .data_violations
            .iter()
            .any(|v| v.detail.contains("replicaCount")),
        "test_edit surfaces the existing out-of-range value before commit: {impact:?}"
    );
}
use super::*;
