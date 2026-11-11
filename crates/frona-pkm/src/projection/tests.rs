use super::*;
use crate::classify::EntityProposal;
use crate::model::EntityCategory;
use crate::model::{KnowledgeEntity, KnowledgeEntityLink, LinkOrigin};

fn entity(id: &str, sources: &[&str]) -> KnowledgeConsolidationEntity {
    let committed = KnowledgeEntity {
        id: id.into(),
        user_id: "user".into(),
        path: "things/example".into(),
        origin: crate::model::EntityOrigin::Internal,
        category: EntityCategory::Concept,
        kinds: Vec::new(),
        name: "Example".into(),
        description: String::new(),
        identity_evidence: Vec::new(),
        attribute_sources: Vec::new(),
        source_memory_ids: sources.iter().map(|source| (*source).into()).collect(),
        body: String::new(),
        sync_content: None,
        mirrored_rev: None,
        extracted_rev: None,
        related_playbooks: Vec::new(),
        search_text: String::new(),
        search_names: Vec::new(),
        search_name_tokens: Vec::new(),
        search_assertions: Vec::new(),
        attributes: serde_json::json!({}),
        use_count: 0,
        aliases: Default::default(),
        rev: None,
        updated_at: chrono::Utc::now(),
        rendered_at: chrono::DateTime::<chrono::Utc>::MIN_UTC,
    };
    let mut entity = KnowledgeConsolidationEntity::from_committed("run", committed);
    entity.entity_id = (!id.is_empty()).then(|| id.to_string());
    entity
}

fn link(relation: &str, target: &str) -> KnowledgeEntityLink {
    KnowledgeEntityLink {
        id: format!("{relation}-{target}"),
        user_id: "user".into(),
        from_entity_path: "things/example".into(),
        to_entity_path: target.into(),
        relation: relation.into(),
        source_memory_ids: vec!["memory-1".into()],
        origin: LinkOrigin::Asserted,
        created_at: chrono::Utc::now(),
    }
}

#[test]
fn only_new_entities_without_memory_are_shells() {
    assert!(is_unbacked_shell(&entity("", &[])));
    assert!(!is_unbacked_shell(&entity("", &["memory-1"])));
    assert!(!is_unbacked_shell(&entity("stored-entity", &[])));
}

#[test]
fn promoted_relation_makes_a_shell_a_referenced_target() {
    let mut proposals = ProposalSet::default();
    proposals.by_path.insert(
        "people/sarah".into(),
        EntityProposal {
            promoted: vec![(
                "employer".into(),
                "schema:worksFor".into(),
                "organizations/acme".into(),
            )],
            ..EntityProposal::default()
        },
    );

    let targets = proposals.referenced_targets();
    assert!(targets.contains("organizations/acme"));
    assert!(!targets.contains("organizations/other"));
}

#[test]
fn projected_graph_removes_only_the_explicitly_retracted_relation() {
    let mut proposals = ProposalSet::default();
    proposals.by_path.insert(
        "things/example".into(),
        EntityProposal {
            retracted: vec![("schema:memberOf".into(), "organizations/old".into())],
            ..EntityProposal::default()
        },
    );
    let links = vec![
        link("schema:memberOf", "organizations/old"),
        link("schema:memberOf", "organizations/current"),
        link("schema:knows", "people/friend"),
    ];

    let (_, projected_links) = proposals.project_graph(
        "user",
        vec![entity("entity-1", &["memory-1"]).as_knowledge_entity()],
        links,
    );

    assert!(!projected_links.iter().any(|link| {
        link.relation == "schema:memberOf" && link.to_entity_path == "organizations/old"
    }));
    assert!(projected_links.iter().any(|link| {
        link.relation == "schema:memberOf" && link.to_entity_path == "organizations/current"
    }));
    assert!(
        projected_links.iter().any(|link| {
            link.relation == "schema:knows" && link.to_entity_path == "people/friend"
        })
    );
}

#[test]
fn reconcile_minted_attribute_enters_the_tbox_proposal_layer() {
    let mut proposals = ProposalSet::default();
    proposals.record(
        "people/me",
        EntityProposal {
            classes: vec!["schema:Person".into()],
            ..EntityProposal::default()
        },
    );
    let px = crate::ontology::PrefixMap::standard();

    proposals.add_reconcile_attributes(
        "people/me",
        &serde_json::json!({"frona:securityPreference": "secure connections"}),
        &[],
        &px,
    );

    assert!(proposals.proposed_edits.iter().any(|edit| matches!(
        edit,
        crate::ontology::SchemaEdit::DeclareDataProperty { property }
            if property == "frona:securityPreference"
    )));
}
