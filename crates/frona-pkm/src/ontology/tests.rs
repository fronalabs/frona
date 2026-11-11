use super::*;
use crate::model::{
    ENTITY_NAME_PROPERTY_IRI, ENTITY_PATH_PROPERTY_IRI, EntityCategory,
    KnowledgeConsolidationEntity, LinkOrigin,
};
use catalogue::roots::Root;

fn catalogue() -> Arc<OntologyCatalogue> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("base.ttl"),
        "<https://example.test/Unused> a <http://www.w3.org/2002/07/owl#Class> .",
    )
    .unwrap();
    OntologyCatalogue::load(&[(Root::User, dir.path())]).unwrap()
}

fn state(owner: &str) -> KnowledgeOntology {
    KnowledgeOntology {
        id: "ontology".into(),
        user_id: owner.into(),
        owl: String::new(),
        format: DELTA_FORMAT.into(),
        version: 7,
        effective_ontology: String::new(),
        seeds: vec![],
        sources: vec![],
        catalog_fingerprint: String::new(),
        updated_at: chrono::Utc::now(),
    }
}
fn concept(path: &str, kind: &str) -> KnowledgeConsolidationEntity {
    let mut row = KnowledgeConsolidationEntity::pending(
        "run",
        "owner",
        path,
        EntityCategory::Concept,
        vec![],
        Default::default(),
    );
    row.kinds = vec![kind.into()];
    row
}
fn link(relation: &str, origin: LinkOrigin) -> KnowledgeEntityLink {
    KnowledgeEntityLink {
        id: relation.into(),
        user_id: "owner".into(),
        from_entity_path: "a".into(),
        to_entity_path: "b".into(),
        relation: relation.into(),
        source_memory_ids: vec![],
        origin,
        created_at: chrono::Utc::now(),
    }
}
#[tokio::test]
async fn snapshot_seeds_match_server_metadata_and_asserted_link_rules() {
    let catalogue = catalogue();
    let graph = GraphSnapshot {
        scope_id: "owner".into(),
        entities: vec![concept("a", "schema:Person")],
        links: vec![
            link("https://example.test/asserted", LinkOrigin::Asserted),
            link("https://example.test/inferred", LinkOrigin::Inferred),
        ],
        ..Default::default()
    };
    let ontology = Ontology::new(catalogue, state("owner"), graph);
    let loaded = ontology.load("owner").await.unwrap();
    let seeds = loaded.effective_ontology().seeds();
    assert!(seeds.contains(&ENTITY_NAME_PROPERTY_IRI.to_string()));
    assert!(seeds.contains(&ENTITY_PATH_PROPERTY_IRI.to_string()));
    assert!(seeds.contains(&"https://example.test/asserted".to_string()));
    assert!(!seeds.contains(&"https://example.test/inferred".to_string()));
}
#[tokio::test]
async fn snapshot_rejects_a_different_requested_or_stored_scope() {
    let catalogue = catalogue();
    let graph = GraphSnapshot {
        scope_id: "owner".into(),
        ..Default::default()
    };
    let ontology = Ontology::new(catalogue.clone(), state("owner"), graph.clone());
    assert!(ontology.load("different-owner").await.is_err());
    assert!(ontology.plan_schema("different-owner", &[]).await.is_err());
    assert!(
        ontology
            .plan_retype("different-owner", "a", "b", &[])
            .await
            .is_err()
    );
    let ontology = Ontology::new(catalogue, state("different-owner"), graph);
    assert!(ontology.load("owner").await.is_err());
}
#[tokio::test]
async fn removing_catalogue_source_preserves_still_referenced_persisted_axioms() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("local.ttl"),
        r#"
 @prefix owl: <http://www.w3.org/2002/07/owl#> .
 @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
 <https://example.test/Old> a owl:Class; rdfs:subClassOf <https://example.test/Parent> .
 <https://example.test/Parent> a owl:Class .
 "#,
    )
    .unwrap();
    let catalogue = OntologyCatalogue::load(&[(Root::User, dir.path())]).unwrap();
    let graph = GraphSnapshot {
        scope_id: "owner".into(),
        entities: vec![concept("a", "https://example.test/Old")],
        ..Default::default()
    };
    let before = Ontology::new(catalogue.clone(), state("owner"), graph.clone())
        .load("owner")
        .await
        .unwrap();
    let scope = before.effective_ontology();
    let mut persisted = state("owner");
    persisted.effective_ontology = scope.to_ntriples();
    persisted.seeds = scope.seeds().to_vec();
    persisted.sources = scope.sources().to_vec();
    persisted.catalog_fingerprint = catalogue.fingerprint().into();
    let after = Ontology::new(self::catalogue(), persisted, graph)
        .load("owner")
        .await
        .unwrap();
    assert_eq!(after.version(), 7);
    assert!(
        after
            .effective_ontology()
            .describes("https://example.test/Old")
    );
    assert!(
        after
            .effective_ontology()
            .triples()
            .iter()
            .any(
                |triple| triple.subject.to_string() == "<https://example.test/Old>"
                    && triple.predicate.as_str() == RDFS_SUBCLASS_OF
                    && triple.object.to_string() == "<https://example.test/Parent>"
            )
    );
}
#[tokio::test]
async fn schema_planning_returns_a_proposal_without_mutating_snapshot_state() {
    let ontology = Ontology::new(
        catalogue(),
        state("owner"),
        GraphSnapshot {
            scope_id: "owner".into(),
            ..Default::default()
        },
    );
    let planned = ontology
        .plan_schema(
            "owner",
            &[SchemaEdit::DeclareClass {
                class: "frona:Example".into(),
            }],
        )
        .await
        .unwrap();
    assert!(planned.owl.contains("Example"));
    assert_eq!(planned.version, 7);
    assert!(ontology.state.owl.is_empty());
    assert_eq!(ontology.load("owner").await.unwrap().delta_ofn(), "");
}

#[tokio::test]
async fn reasoning_does_not_treat_previous_inferences_as_assertions() {
    let ontology = Ontology::new(
        catalogue(),
        state("owner"),
        GraphSnapshot {
            scope_id: "owner".into(),
            entities: vec![
                concept("a", "https://example.test/Unused"),
                concept("b", "https://example.test/Unused"),
            ],
            links: vec![
                link("https://example.test/asserted", LinkOrigin::Asserted),
                link("https://example.test/inferred", LinkOrigin::Inferred),
            ],
            ..Default::default()
        },
    );
    let pass = ontology.reason_user("owner").await.unwrap();
    assert_eq!(pass.asserted_links.len(), 1);
    assert_eq!(pass.asserted_links[0].origin, LinkOrigin::Asserted);
    let proposed = ontology
        .reason_user_with_proposed("owner", &[], &[])
        .await
        .unwrap();
    assert_eq!(proposed.asserted_links.len(), 1);
}

#[tokio::test]
async fn reasoning_returns_new_inferred_edges_without_rewriting_snapshot() {
    let graph = GraphSnapshot {
        scope_id: "owner".into(),
        entities: vec![
            concept("a", "https://example.test/Unused"),
            concept("b", "https://example.test/Unused"),
        ],
        links: vec![link("urn:frona:knows", LinkOrigin::Asserted)],
        ..Default::default()
    };
    let mut state = state("owner");
    state.owl="Ontology(Declaration(ObjectProperty(<urn:frona:knows>)) SymmetricObjectProperty(<urn:frona:knows>))".into();
    let ontology = Ontology::new(catalogue(), state, graph);
    let pass: crate::ontology::ReasonPass = ontology.reason_user("owner").await.unwrap();
    let inferred: crate::ontology::InferredGraph = pass.inferred_graph().unwrap();
    assert_eq!(
        inferred.links,
        vec![("b".into(), "a".into(), "frona:knows".into())]
    );
    assert!(inferred.same_as.is_empty());
    assert_eq!(ontology.graph().links.len(), 1);
    assert_eq!(ontology.graph().links[0].from_entity_path, "a");
}
