use super::*;
use std::collections::HashSet;

#[tokio::test]
async fn inspection_combines_catalogue_and_proposed_class_hierarchy() {
    let (mgr, _repo) = manager().await;
    let proposed = vec![
        SchemaEdit::DeclareClass {
            class: "frona:Dog".into(),
        },
        SchemaEdit::SubClassOf {
            sub: "frona:Dog".into(),
            sup: "schema:Person".into(),
        },
    ];
    let terms = vec!["frona:Dog".to_string(), "schema:Person".to_string()];
    let (inspections, relations) = mgr
        .portable("u")
        .await
        .unwrap()
        .inspect_ontology_terms("u", &proposed, &HashSet::new(), &terms)
        .await
        .unwrap();

    let dog = &inspections[0];
    assert!(
        dog.exists,
        "the proposed class exists in the combined schema view"
    );
    assert_eq!(dog.kind.as_deref(), Some("class"));
    assert_eq!(dog.direct_parents, ["schema:Person"]);
    assert!(
        dog.ancestors.contains(&"schema:Thing".to_string()),
        "catalogue ancestors included"
    );
    assert_eq!(dog.user_relevance, "proposed");
    assert_eq!(relations[0].relation, "subclass");
}

#[tokio::test]
async fn search_prefers_an_active_user_term_for_an_equal_text_match() {
    let (mgr, _repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::DeclareClass {
            class: "frona:Person".into(),
        }],
    )
    .await
    .unwrap();
    let active = HashSet::from([iri("frona:Person")]);
    let hits = mgr
        .portable("u")
        .await
        .unwrap()
        .search_ontology_terms("u", &[], &active, "person", 10)
        .await
        .unwrap();

    assert_eq!(
        hits.first().map(|hit| hit.term.as_str()),
        Some("frona:Person")
    );
    assert_eq!(hits[0].user_relevance, "directly_used");
    assert!(
        hits.iter().any(|hit| hit.term == "schema:Person"),
        "catalogue match remains visible"
    );
}

#[tokio::test]
async fn exact_catalogue_match_stays_above_an_active_partial_match() {
    let (mgr, _repo) = manager().await;
    mgr.commit(
        "u",
        &[SchemaEdit::DeclareClass {
            class: "frona:PersonRecord".into(),
        }],
    )
    .await
    .unwrap();
    let active = HashSet::from([iri("frona:PersonRecord")]);
    let hits = mgr
        .portable("u")
        .await
        .unwrap()
        .search_ontology_terms("u", &[], &active, "person", 10)
        .await
        .unwrap();

    assert_eq!(
        hits.first().map(|hit| hit.term.as_str()),
        Some("schema:Person")
    );
}

#[tokio::test]
async fn property_inspection_returns_user_schema_structure() {
    let (mgr, _repo) = manager().await;
    let proposed = vec![
        SchemaEdit::DeclareObjectProperty {
            property: "frona:ownsPet".into(),
        },
        SchemaEdit::ObjectPropertyDomain {
            property: "frona:ownsPet".into(),
            class: "schema:Person".into(),
        },
        SchemaEdit::ObjectPropertyRange {
            property: "frona:ownsPet".into(),
            class: "schema:Thing".into(),
        },
        SchemaEdit::DeclareObjectProperty {
            property: "frona:ownedBy".into(),
        },
        SchemaEdit::InverseProperties {
            a: "frona:ownsPet".into(),
            b: "frona:ownedBy".into(),
        },
    ];
    let (inspections, _) = mgr
        .portable("u")
        .await
        .unwrap()
        .inspect_ontology_terms(
            "u",
            &proposed,
            &HashSet::new(),
            &["frona:ownsPet".to_string()],
        )
        .await
        .unwrap();

    let property = inspections[0]
        .property
        .as_ref()
        .expect("object property details");
    assert_eq!(property.domain, ["schema:Person"]);
    assert_eq!(property.range, ["schema:Thing"]);
    assert_eq!(property.inverse, ["frona:ownedBy"]);
}
