use super::*;
use crate::model::{ConsolidationEntityLifecycle, EntityCategory};
fn row(path: &str) -> KnowledgeConsolidationEntity {
    KnowledgeConsolidationEntity::pending(
        "run",
        "owner",
        path,
        EntityCategory::Concept,
        vec![],
        Default::default(),
    )
}
#[test]
fn mixed_base_and_draft_redirect_cycles_are_rejected() {
    let active = row("a");
    let mut back = row("b");
    back.mark_coalesced("a");
    let graph = GraphSnapshot {
        entities: vec![active, back],
        ..Default::default()
    };
    let mut forward = row("a");
    forward.mark_coalesced("b");
    assert!(
        graph
            .entity_by_path_with(&EntityDraft::from_rows([forward]), "a")
            .is_err()
    );
}
#[test]
fn base_redirect_resolves_to_draft_replacement() {
    let mut old = row("a");
    old.mark_coalesced("b");
    let mut target = row("b");
    target.name = "old name".into();
    let graph = GraphSnapshot {
        entities: vec![old, target],
        ..Default::default()
    };
    let mut replacement = row("b");
    replacement.name = "new name".into();
    let found = graph
        .entity_by_path_with(&EntityDraft::from_rows([replacement]), "a")
        .unwrap()
        .unwrap();
    assert_eq!(found.path, "b");
    assert_eq!(found.name, "new name");
}
#[test]
fn redirects_cannot_resurrect_a_discarded_draft_target() {
    let mut source = row("a");
    source.mark_coalesced("b");
    let graph = GraphSnapshot {
        entities: vec![source, row("b")],
        ..Default::default()
    };
    let mut deleted = row("b");
    deleted.lifecycle = ConsolidationEntityLifecycle::Discarded;
    assert!(
        graph
            .entity_by_path_with(&EntityDraft::from_rows([deleted]), "a")
            .unwrap()
            .is_none()
    );
}
#[test]
fn missing_redirect_targets_are_not_entities() {
    let mut source = row("a");
    source.mark_coalesced("missing");
    let graph = GraphSnapshot {
        entities: vec![source],
        ..Default::default()
    };
    assert!(graph.entity_by_path("a").unwrap().is_none());
}
