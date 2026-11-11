use super::*;
use crate::model::{ConsolidationEntityLifecycle, EntityCategory, KnowledgeConsolidationEntity};

fn entity(path: &str) -> KnowledgeConsolidationEntity {
    let mut entity = KnowledgeConsolidationEntity::pending(
        "run",
        "user",
        path,
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    entity.name = path.to_string();
    entity.rederive_search();
    entity
}

#[test]
fn draft_replacement_shadows_durable_state() {
    let mut durable = entity("people/casey-owner");
    durable.name = "Old Casey Owner".into();
    let mut replacement = entity("people/casey-owner");
    replacement.name = "Current Casey Owner".into();
    let draft = EntityDraft::from_rows([replacement]);

    let snapshot = EntitySnapshot::new([durable], &draft);

    assert_eq!(
        snapshot.entity_by_path("people/casey-owner").unwrap().name,
        "Current Casey Owner"
    );
}

#[test]
fn draft_tombstone_suppresses_durable_state() {
    let durable = entity("people/casey-owner");
    let mut discarded = entity("people/casey-owner");
    discarded.mark_discarded("not an entity");
    let draft = EntityDraft::from_rows([discarded]);

    let snapshot = EntitySnapshot::new([durable], &draft);

    assert!(snapshot.entity_by_path("people/casey-owner").is_none());
}

#[test]
fn draft_redirect_cycle_fails_safely() {
    let mut first = entity("people/first");
    first.mark_coalesced("people/second");
    let mut second = entity("people/second");
    second.mark_coalesced("people/first");
    let draft = EntityDraft::from_rows([first, second]);

    assert!(draft.resolve("people/first").is_err());
}

#[test]
fn draft_lifecycle_helpers_preserve_storage_invariants() {
    let mut discarded = entity("people/discarded");
    discarded.mark_discarded("duplicate");
    assert_eq!(discarded.lifecycle, ConsolidationEntityLifecycle::Discarded);
    assert!(discarded.validate().is_ok());

    let mut coalesced = entity("people/old");
    coalesced.mark_coalesced("people/current");
    assert_eq!(coalesced.lifecycle, ConsolidationEntityLifecycle::Coalesced);
    assert!(coalesced.validate().is_ok());
}
