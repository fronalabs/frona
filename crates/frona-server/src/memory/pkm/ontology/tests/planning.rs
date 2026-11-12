/// Arriving with something more specific retires the general one - the entity is
/// still a `Person`, the reasoner just derives it rather than storing it.
#[tokio::test]
async fn more_specific_type_retires_the_one_it_implies() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/sarah", "schema:Thing").await;
    mgr.commit(
        "u",
        &[SchemaEdit::SubClassOf {
            sub: "frona:Engineer".into(),
            sup: "schema:Person".into(),
        }],
    )
    .await
    .unwrap();

    assert!(stamp(&mgr, &repo, "people/sarah", "frona:Engineer").await);
    let entity = repo
        .entity_by_path("u", "people/sarah")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entity.kinds,
        ["urn:frona:Engineer"],
        "Thing and Person are both implied"
    );
}

/// An entity is several things at once, and each type is asserted independently.
/// The two here are genuinely independent - neither implies the other, so both
/// survive normalisation and both have to reach the reasoner.
#[tokio::test]
async fn entity_carries_every_compatible_type() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "svc/pg", "schema:SoftwareApplication").await;
    assert!(stamp(&mgr, &repo, "svc/pg", "frona:Database").await);

    let entity = repo.entity_by_path("u", "svc/pg").await.unwrap().unwrap();
    assert_eq!(
        entity.kinds,
        [
            "https://schema.org/SoftwareApplication",
            "urn:frona:Database"
        ]
    );

    let pass = mgr.reason_user("u").await.unwrap();
    let pg = individual_iri("svc/pg");
    for want in [
        "https://schema.org/SoftwareApplication",
        "urn:frona:Database",
    ] {
        let q = format!("ASK {{ <{pg}> a <{want}> }}");
        assert!(
            sparql::ask(&pass.reasoned.store, &q, pass.effective_ontology.prefixes()).unwrap(),
            "asserted {want}"
        );
    }
    let q = format!("ASK {{ <{pg}> a <https://schema.org/Thing> }}");
    assert!(
        sparql::ask(&pass.reasoned.store, &q, pass.effective_ontology.prefixes()).unwrap(),
        "Thing is inferred, not stored"
    );
}

/// The gate. A type that contradicts one the entity already carries is refused; the
/// entity keeps what it had, and **every fact survives** - a classification error is
/// a classification error, not a reason to quarantine data.
#[tokio::test]
async fn clashing_type_is_rejected_and_the_facts_are_untouched() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    seed_reconciled_entity(
        &repo,
        "u",
        "orgs/acme",
        "",
        "Acme, the retailer",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    let before = repo
        .entity_by_path("u", "orgs/acme")
        .await
        .unwrap()
        .unwrap();

    assert!(
        !stamp(&mgr, &repo, "orgs/acme", "schema:Person").await,
        "the contradiction is caught and the arrival refused"
    );

    let after = repo
        .entity_by_path("u", "orgs/acme")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.kinds,
        ["https://schema.org/Organization"],
        "the newest type lost"
    );
    assert_eq!(
        after.body, before.body,
        "and the entity's facts are untouched"
    );
}

#[tokio::test]
async fn graph_validation_reports_candidate_witnesses() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    let mut entities = repo.list_entities("u").await.unwrap();
    entities[0]
        .kinds
        .push(PrefixMap::standard().expand("schema:Person"));
    let report = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(!report.is_valid());
    assert!(
        report.diagnostics.iter().any(|diagnostic| {
            diagnostic.subject.as_deref() == Some("orgs/acme")
                && !diagnostic.witness_triples.is_empty()
        }),
        "{:#?}",
        report.diagnostics
    );
}

/// A standard property newly used by a staged A-Box may not appear in the user's
/// committed ontology seed set yet. Its catalogue definition must still participate
/// in the same validation pass, or the first assertion is judged as unconstrained
/// and only becomes invalid after commit/reload.
#[tokio::test]
async fn graph_validation_admits_terms_from_the_staged_abox() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    seed_concept(&repo, "u", "places/office", "schema:Place").await;
    let entities = repo.list_entities("u").await.unwrap();
    let links = [KnowledgeEntityLink {
        id: "staged-link".into(),
        user_id: "u".into(),
        from_entity_path: "orgs/acme".into(),
        to_entity_path: "places/office".into(),
        relation: "http://example.org/strict/personLocation".into(),
        source_memory_ids: Vec::new(),
        origin: LinkOrigin::Asserted,
        created_at: chrono::Utc::now(),
    }];

    let report = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &links.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        !report.is_valid(),
        "the staged predicate's strict domain must be in scope"
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.subject.as_deref() == Some("orgs/acme") }),
        "{:#?}",
        report.diagnostics
    );

    seed_asserted_entity_link(
        &repo,
        "u",
        "orgs/acme",
        "places/office",
        "http://example.org/strict/personLocation",
    )
    .await
    .unwrap();
    let restarted = OntologyManager::new(roots(), repo);
    let pass = restarted.reason_user("u").await.unwrap();
    let after_reload = restarted.validate(&pass);
    assert!(
        after_reload
            .iter()
            .any(|violation| { violation.subject.as_deref() == Some("orgs/acme") }),
        "validation must not change after the staged term becomes committed"
    );
}

#[tokio::test]
async fn schema_includes_remain_advisory_after_catalogue_loading() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    seed_concept(&repo, "u", "places/office", "schema:Place").await;
    let entities = repo.list_entities("u").await.unwrap();
    let links = [KnowledgeEntityLink {
        id: "advisory-link".into(),
        user_id: "u".into(),
        from_entity_path: "orgs/acme".into(),
        to_entity_path: "places/office".into(),
        relation: "http://example.org/strict/advisoryLocation".into(),
        source_memory_ids: Vec::new(),
        origin: LinkOrigin::Asserted,
        created_at: chrono::Utc::now(),
    }];

    let report = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &links.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        report.is_valid(),
        "schema:domainIncludes must not constrain the A-Box: {:#?}",
        report.diagnostics
    );
}

#[tokio::test]
async fn graph_validation_admits_classes_from_the_staged_abox() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    let mut entities = repo.list_entities("u").await.unwrap();
    entities[0]
        .kinds
        .push("http://example.org/strict/Employee".into());

    let report = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        !report.is_valid(),
        "the staged class hierarchy must be in scope"
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.subject.as_deref() == Some("orgs/acme") }),
        "{:#?}",
        report.diagnostics
    );
}

#[tokio::test]
async fn graph_validation_admits_data_properties_from_the_staged_abox() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Organization").await;
    let mut entities = repo.list_entities("u").await.unwrap();
    entities[0].attributes = serde_json::json!({
        "http://example.org/strict/personCode": "A-17",
    });

    let report = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        !report.is_valid(),
        "the staged data property's strict domain must be in scope"
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.subject.as_deref() == Some("orgs/acme") }),
        "{:#?}",
        report.diagnostics
    );
}

#[tokio::test]
async fn graph_validation_never_allows_undeclared_custom_usage() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "services/db", "frona:Database").await;
    let entities = repo.list_entities("u").await.unwrap();
    let invalid = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        invalid.diagnostics.iter().any(|diagnostic| {
            matches!(diagnostic.kind, ValidationDiagnosticKind::UndeclaredTerm)
        })
    );
    let valid = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
            &[SchemaEdit::SubClassOf {
                sub: "frona:Database".into(),
                sup: "schema:SoftwareApplication".into(),
            }],
        )
        .await
        .unwrap();
    assert!(valid.is_valid(), "{:#?}", valid.diagnostics);
}

#[tokio::test]
async fn graph_validation_ignores_links_that_do_not_enter_the_abox() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "devices/tool", "schema:Product").await;
    repo.upsert_entity_skeleton(
        "u",
        "procedures/update-tool",
        EntityCategory::Playbook,
        &[iri("schema:Thing")],
        "Update tool",
        "",
        &[],
    )
    .await
    .unwrap();
    let entities = repo.list_entities("u").await.unwrap();
    let links = [KnowledgeEntityLink {
        id: "playbook-link".into(),
        user_id: "u".into(),
        from_entity_path: "devices/tool".into(),
        to_entity_path: "procedures/update-tool".into(),
        relation: "frona:playbook".into(),
        source_memory_ids: Vec::new(),
        origin: LinkOrigin::Asserted,
        created_at: chrono::Utc::now(),
    }];

    let report = mgr
        .portable("u")
        .await
        .unwrap()
        .validate_graph(
            "u",
            &entities.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &links.iter().cloned().map(Into::into).collect::<Vec<_>>(),
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        report.is_valid(),
        "navigation-only links are not ontology assertions: {:#?}",
        report.diagnostics
    );
}

/// The refusal is about *contradiction*, not novelty - an unrelated second type is
/// admitted, or multi-typing would be multi-typing in name only.
#[tokio::test]
async fn unrelated_type_is_admitted() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "svc/pg", "schema:SoftwareApplication").await;
    assert!(stamp(&mgr, &repo, "svc/pg", "frona:Database").await);

    let entity = repo.entity_by_path("u", "svc/pg").await.unwrap().unwrap();
    assert_eq!(
        entity.kinds,
        [
            "https://schema.org/SoftwareApplication",
            "urn:frona:Database"
        ]
    );
}

#[tokio::test]
async fn re_adding_an_existing_type_changes_nothing() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "people/bob", "schema:Person").await;
    assert!(
        stamp(&mgr, &repo, "people/bob", "schema:Person").await,
        "already held"
    );
    let entity = repo
        .entity_by_path("u", "people/bob")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.kinds, ["https://schema.org/Person"]);
}

/// Aligning one term onto another can land it on an entity that already carries
/// something implying it. Normalising at the swap keeps "what is stored is the most
/// specific set" true, so no later stage has to clean up after an alignment.
#[tokio::test]
async fn alignment_normalizes_the_entities_it_moves() {
    let (mgr, repo) = manager().await;
    seed_concept(&repo, "u", "orgs/acme", "schema:Corporation").await;
    assert!(stamp(&mgr, &repo, "orgs/acme", "frona:Company").await);
    assert_eq!(
        repo.entity_by_path("u", "orgs/acme")
            .await
            .unwrap()
            .unwrap()
            .kinds,
        [iri("schema:Corporation"), "urn:frona:Company".to_string()],
        "unrelated so far, so both stand"
    );

    mgr.commit(
        "u",
        &[SchemaEdit::SubClassOf {
            sub: "schema:Corporation".into(),
            sup: "schema:Organization".into(),
        }],
    )
    .await
    .unwrap();
    let planned = mgr
        .portable("u")
        .await
        .unwrap()
        .plan_schema("u", &[])
        .await
        .unwrap();
    let moved = mgr
        .portable("u")
        .await
        .unwrap()
        .plan_retype(
            "u",
            "urn:frona:Company",
            &iri("schema:Organization"),
            &planned.triples,
        )
        .await
        .unwrap();
    assert_eq!(moved.len(), 1);
    repo.commit_schema_and_types(
        "u",
        &planned.owl,
        DELTA_FORMAT,
        planned.version,
        &moved,
        &[],
        &[],
        &[],
        &[],
        &[],
        &[],
        &[],
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        repo.entity_by_path("u", "orgs/acme")
            .await
            .unwrap()
            .unwrap()
            .kinds,
        [iri("schema:Corporation")],
        "Organization arrived, and is implied by Corporation, so it is not stored"
    );
}
use super::*;
