#[tokio::test]
async fn suspect_excludes_but_remintable_and_reversible() {
    let r = repo().await;
    let mid = r
        .create_sourced_memory(
            "u",
            MemoryKind::Fact,
            "acme is a person",
            &["organizations/acme".into()],
            human_evidence("acme is a person", "organizations/acme"),
        )
        .await
        .unwrap();

    r.set_disposition("u", &mid, Disposition::Suspect)
        .await
        .unwrap();
    let mems = r
        .memories_for_entity("u", "organizations/acme")
        .await
        .unwrap();
    assert_eq!(mems[0].disposition, Disposition::Suspect);

    let (cur, hist) = classify_memories(&mems);
    assert!(cur.is_empty() && hist.is_empty(), "suspect appears nowhere");

    let err = r
        .erroneous_contents_for_entity("u", "organizations/acme")
        .await
        .unwrap();
    assert!(err.is_empty(), "suspect is not erroneous (re-mintable)");

    r.set_disposition("u", &mid, Disposition::None)
        .await
        .unwrap();
    let mems = r
        .memories_for_entity("u", "organizations/acme")
        .await
        .unwrap();
    assert_eq!(mems[0].disposition, Disposition::None);
    assert_eq!(classify_memories(&mems).0.len(), 1, "reinstated → current");
}

async fn page_with_attributes(r: &PkmRepo, path: &str, attrs: serde_json::Value) {
    r.upsert_entity_skeleton(
        "u",
        path,
        EntityCategory::Concept,
        &[],
        path.rsplit('/').next().unwrap_or(path),
        "",
        &[],
    )
    .await
    .unwrap();
    seed_reconciled_entity(r, "u", path, "", "", &attrs)
        .await
        .unwrap();
}

/// Extract states a fact as a literal because it cannot tell an entity from a string;
/// the Classify stage decides the value names an entity. The literal must **become** the
/// edge, not sit alongside it - storing it twice, in two shapes, with nothing keeping
/// them in step, is the state this whole path exists to remove.
#[tokio::test]
async fn promoted_attribute_becomes_an_edge_and_stops_being_a_literal() {
    let r = repo().await;
    page_with_attributes(
        &r,
        "people/me",
        serde_json::json!({ "employer": "Example Corp", "role": "Engineer" }),
    )
    .await;
    page_with_attributes(&r, "organizations/example-corp", serde_json::json!({})).await;

    let ops = AttributeOps {
        path: "people/me".into(),
        rekeys: vec![("role".into(), "schema:jobTitle".into())],
        promoted: vec![(
            "employer".into(),
            "schema:worksFor".into(),
            "organizations/example-corp".into(),
        )],
        retracted: Vec::new(),
    };
    assert!(
        r.commit_schema_and_types(
            "u",
            "Ontology()",
            "ofn",
            0,
            &[],
            &[],
            &[],
            &[ops],
            &[],
            &[],
            &[],
            &[],
            None
        )
        .await
        .unwrap()
    );

    let entity = r.entity_by_path("u", "people/me").await.unwrap().unwrap();
    let attrs = entity.attributes.as_object().unwrap();
    assert!(
        !attrs.contains_key("employer"),
        "the literal is gone: {attrs:?}"
    );
    assert_eq!(
        attrs.get("schema:jobTitle").and_then(|v| v.as_str()),
        Some("Engineer"),
        "the one that stayed a literal kept its value under its CURIE: {attrs:?}"
    );
    assert!(
        !attrs.contains_key("role"),
        "under its CURIE, not both: {attrs:?}"
    );

    let links = r.links_from_entity("u", "people/me").await.unwrap();
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(links[0].to_entity_path, "organizations/example-corp");
    assert_eq!(links[0].relation, "schema:worksFor");
    assert_eq!(
        links[0].origin,
        LinkOrigin::Asserted,
        "stated, not inferred"
    );
}

/// Re-running the same decision must not accumulate edges. Consolidation is resumable
/// and re-entrant, so every write in this path is reached more than once.
#[tokio::test]
async fn promoting_the_same_attribute_twice_writes_one_edge() {
    let r = repo().await;
    page_with_attributes(
        &r,
        "people/me",
        serde_json::json!({ "employer": "Example Corp" }),
    )
    .await;
    page_with_attributes(&r, "organizations/example-corp", serde_json::json!({})).await;
    let ops = || AttributeOps {
        path: "people/me".into(),
        rekeys: Vec::new(),
        promoted: vec![(
            "employer".into(),
            "schema:worksFor".into(),
            "organizations/example-corp".into(),
        )],
        retracted: Vec::new(),
    };
    for version in 0..2 {
        r.commit_schema_and_types(
            "u",
            "Ontology()",
            "ofn",
            version,
            &[],
            &[],
            &[],
            &[ops()],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        r.links_from_entity("u", "people/me").await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn replacement_retracts_only_the_old_edge_with_the_same_property() {
    let r = repo().await;
    page_with_attributes(&r, "people/me", serde_json::json!({})).await;
    page_with_attributes(&r, "organizations/former-corp", serde_json::json!({})).await;
    page_with_attributes(&r, "organizations/example-corp", serde_json::json!({})).await;
    seed_asserted_entity_link(
        &r,
        "u",
        "people/me",
        "organizations/former-corp",
        "schema:worksFor",
    )
    .await
    .unwrap();
    seed_asserted_entity_link(
        &r,
        "u",
        "people/me",
        "organizations/former-corp",
        "frona:formerEmployer",
    )
    .await
    .unwrap();

    let ops = AttributeOps {
        path: "people/me".into(),
        rekeys: Vec::new(),
        promoted: vec![(
            "schema:worksFor".into(),
            "schema:worksFor".into(),
            "organizations/example-corp".into(),
        )],
        retracted: vec![("schema:worksFor".into(), "organizations/former-corp".into())],
    };
    r.commit_schema_and_types(
        "u",
        "Ontology()",
        "ofn",
        0,
        &[],
        &[],
        &[],
        &[ops],
        &[],
        &[],
        &[],
        &[],
        None,
    )
    .await
    .unwrap();

    let links = r.links_from_entity("u", "people/me").await.unwrap();
    assert!(links.iter().any(|l| {
        l.relation == "schema:worksFor" && l.to_entity_path == "organizations/example-corp"
    }));
    assert!(!links.iter().any(|l| {
        l.relation == "schema:worksFor" && l.to_entity_path == "organizations/former-corp"
    }));
    assert!(links.iter().any(|l| {
        l.relation == "frona:formerEmployer" && l.to_entity_path == "organizations/former-corp"
    }));
}

/// Resolve merges duplicate mentions between the proposal and the commit, so the entity
/// a decision was made about may be gone by the time it lands. That is not an error -
/// its attributes went with it.
#[tokio::test]
async fn attribute_ops_for_a_vanished_entity_are_skipped_not_fatal() {
    let r = repo().await;
    let ops = AttributeOps {
        path: "people/never-existed".into(),
        rekeys: vec![("a".into(), "frona:a".into())],
        promoted: Vec::new(),
        retracted: Vec::new(),
    };
    assert!(
        r.commit_schema_and_types(
            "u",
            "Ontology()",
            "ofn",
            0,
            &[],
            &[],
            &[],
            &[ops],
            &[],
            &[],
            &[],
            &[],
            None
        )
        .await
        .unwrap(),
        "the commit still succeeds"
    );
}
use super::*;
