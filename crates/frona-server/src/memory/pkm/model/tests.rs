use super::*;
use crate::memory::pkm::ConsolidationStats;

/// `relations` is a slice of `(relation, survivor-id)` recorded on this (subordinate)
/// memory. `classify_memories` reads only the memory's own relations + disposition.
fn mem(id: &str, disposition: Disposition, relations: &[(RelationType, &str)]) -> KnowledgeMemory {
    KnowledgeMemory {
        id: id.into(),
        user_id: "u".into(),
        created_at: Utc::now(),
        kind: MemoryKind::Fact,
        episode: None,
        content: id.into(),
        relations: relations
            .iter()
            .map(|(relation, to)| MemoryRelation {
                relation: *relation,
                to: RecordId::new("knowledge_memory", to.to_string()),
                note: String::new(),
            })
            .collect(),
        disposition,
        ended_at: None,
        comment: None,
        erroneous_at: None,
        evidence: vec![MemoryEvidence {
            strength: EvidenceStrength::Explicit,
            source: EvidenceSource::AgentMessage {
                message_id: "m".into(),
                agent_id: "a".into(),
                chat_id: "c".into(),
                quote: id.into(),
            },
        }],
    }
}

fn ids(ms: &[&KnowledgeMemory]) -> Vec<String> {
    ms.iter().map(|m| m.id.clone()).collect()
}

fn task_memory(id: &str, task_id: &str, status: EpisodeStatus) -> KnowledgeMemory {
    let mut memory = mem(id, Disposition::None, &[]);
    memory.kind = MemoryKind::Episodic;
    memory.episode = Some(Episode {
        status,
        anchor: TemporalAnchor {
            message: id.into(),
            quote: String::new(),
        },
        duration: None,
        absolute: None,
        resolved_start: None,
        resolved_end: None,
    });
    memory.evidence = vec![MemoryEvidence {
        strength: EvidenceStrength::Explicit,
        source: EvidenceSource::TaskLifecycle {
            message_id: id.into(),
            chat_id: "chat".into(),
            task_id: task_id.into(),
        },
    }];
    memory
}

/// The exact shape three surfaces share. Pinned because two of them render the same
/// entity - the prompt the article is written from, and the `## History` block written
/// into the file - so a change here has to be a deliberate change to both.
#[test]
fn memory_renders_as_one_kind_tagged_bullet() {
    let mut m = mem("id1", Disposition::None, &[]);
    m.content = "postgres runs on 5433".into();
    assert_eq!(
        memory_bullet(&m),
        "- (Fact; evidence: AgentMessage/Explicit) postgres runs on 5433\n"
    );
}

#[test]
fn plain_memory_is_current() {
    let ms = vec![mem("a", Disposition::None, &[])];
    let (cur, hist) = classify_memories(&ms);
    assert_eq!(ids(&cur), ["a"]);
    assert!(hist.is_empty());
}

#[test]
fn terminal_task_event_demotes_its_planned_episode_to_history() {
    let ms = vec![
        task_memory("sun-planned", "task-sun", EpisodeStatus::Planned),
        task_memory("sun-completed", "task-sun", EpisodeStatus::Occurred),
        task_memory("mon-planned", "task-mon", EpisodeStatus::Planned),
        task_memory("mon-completed", "task-mon", EpisodeStatus::Occurred),
    ];

    let (cur, hist) = classify_memories(&ms);

    assert_eq!(ids(&cur), ["sun-completed", "mon-completed"]);
    assert_eq!(ids(&hist), ["sun-planned", "mon-planned"]);
}

#[test]
fn terminal_event_for_another_task_does_not_demote_a_plan() {
    let ms = vec![
        task_memory("sun-planned", "task-sun", EpisodeStatus::Planned),
        task_memory("mon-completed", "task-mon", EpisodeStatus::Occurred),
    ];

    let (cur, hist) = classify_memories(&ms);

    assert_eq!(ids(&cur), ["sun-planned", "mon-completed"]);
    assert!(hist.is_empty());
}

#[test]
fn replace_link_demotes_to_history() {
    let ms = vec![
        mem("a", Disposition::None, &[(RelationType::Replace, "b")]),
        mem("b", Disposition::None, &[]),
    ];
    let (cur, hist) = classify_memories(&ms);
    assert_eq!(ids(&cur), ["b"]);
    assert_eq!(ids(&hist), ["a"]);
}

#[test]
fn duplicate_and_absorbed_are_dropped() {
    let ms = vec![
        mem("a", Disposition::None, &[(RelationType::Duplicate, "b")]),
        mem("c", Disposition::None, &[(RelationType::Absorbed, "b")]),
        mem("b", Disposition::None, &[]),
    ];
    let (cur, hist) = classify_memories(&ms);
    assert_eq!(ids(&cur), ["b"], "only the survivor is current");
    assert!(
        hist.is_empty(),
        "duplicate/absorbed never render as 'do not use' History"
    );
}

#[test]
fn outdated_goes_to_history() {
    let ms = vec![mem("a", Disposition::Outdated, &[])];
    let (cur, hist) = classify_memories(&ms);
    assert!(cur.is_empty(), "outdated is not current");
    assert_eq!(ids(&hist), ["a"]);
}

#[test]
fn erroneous_is_excluded_from_both() {
    let ms = vec![mem("a", Disposition::Erroneous, &[])];
    let (cur, hist) = classify_memories(&ms);
    assert!(
        cur.is_empty() && hist.is_empty(),
        "erroneous appears nowhere"
    );
}

#[test]
fn history_class_wins_over_drop_class() {
    let ms = vec![mem(
        "a",
        Disposition::Outdated,
        &[(RelationType::Duplicate, "b")],
    )];
    let (cur, hist) = classify_memories(&ms);
    assert!(cur.is_empty());
    assert_eq!(
        ids(&hist),
        ["a"],
        "History-class (outdated) beats drop-class (duplicate)"
    );
}

fn consolidation_entity(path: &str) -> KnowledgeConsolidationEntity {
    KnowledgeConsolidationEntity::pending(
        "run",
        "u",
        path,
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    )
}

#[test]
fn knowledge_entity_rejects_an_incomplete_record() {
    let mut value =
        serde_json::to_value(consolidation_entity("services/postgres").as_knowledge_entity())
            .unwrap();
    value.as_object_mut().unwrap().remove("body");

    assert!(serde_json::from_value::<KnowledgeEntity>(value).is_err());
}

#[test]
fn consolidation_entity_rejects_an_incomplete_record() {
    let mut value = serde_json::to_value(consolidation_entity("services/postgres")).unwrap();
    value.as_object_mut().unwrap().remove("progress");

    assert!(serde_json::from_value::<KnowledgeConsolidationEntity>(value).is_err());
}

#[test]
fn knowledge_memory_rejects_an_incomplete_record() {
    let mut value = serde_json::to_value(mem("memory-1", Disposition::None, &[])).unwrap();
    value.as_object_mut().unwrap().remove("relations");

    assert!(serde_json::from_value::<KnowledgeMemory>(value).is_err());
}

#[test]
fn knowledge_ontology_rejects_an_incomplete_record() {
    let mut value = serde_json::to_value(KnowledgeOntology {
        id: "ontology-1".into(),
        user_id: "u".into(),
        owl: "Ontology()".into(),
        format: "ofn".into(),
        version: 0,
        effective_ontology: String::new(),
        seeds: Vec::new(),
        sources: Vec::new(),
        catalog_fingerprint: String::new(),
        updated_at: Utc::now(),
    })
    .unwrap();
    value.as_object_mut().unwrap().remove("effective_ontology");

    assert!(serde_json::from_value::<KnowledgeOntology>(value).is_err());
}

#[test]
fn consolidation_lifecycle_helpers_keep_search_and_redirect_state_consistent() {
    let mut coalesced = consolidation_entity("people/old");
    coalesced.mark_coalesced_with_evidence(
        "people/current",
        Some(serde_json::json!({"reason": "same identity"})),
    );
    assert!(coalesced.validate().is_ok());
    assert!(!coalesced.searchable);
    assert_eq!(coalesced.canonical_path.as_deref(), Some("people/current"));
    assert!(matches!(
        coalesced.progress.identity,
        IdentityProgress::Coalesced {
            evidence: Some(_),
            ..
        }
    ));

    let mut discarded = consolidation_entity("people/not-an-entity");
    discarded.mark_discarded("classification rejected");
    assert!(discarded.validate().is_ok());
    assert!(!discarded.searchable);
    assert!(discarded.effective_entity().is_none());
}

#[test]
fn invalid_lifecycle_combinations_are_rejected() {
    let mut missing_redirect = consolidation_entity("people/old");
    missing_redirect.lifecycle = ConsolidationEntityLifecycle::Coalesced;
    missing_redirect.searchable = false;
    assert!(missing_redirect.validate().is_err());

    let mut active_redirect = consolidation_entity("people/current");
    active_redirect.lifecycle = ConsolidationEntityLifecycle::Active;
    active_redirect.canonical_path = Some("people/other".into());
    assert!(active_redirect.validate().is_err());
}

#[test]
fn contribution_merge_preserves_grounding_and_rederives_search_fields() {
    let mut row = consolidation_entity("services/postgres");
    let contribution = PendingEntityContribution {
        name: "Postgres".into(),
        description: "Relational database".into(),
        aliases: ["PostgreSQL".to_string()].into_iter().collect(),
        attributes: serde_json::json!({"version": "17"}),
        attribute_evidence: Default::default(),
        source_memory_ids: ["memory-1".to_string()].into_iter().collect(),
        existing_only: false,
        occurrence_count: 1,
    };
    row.merge_contribution(contribution.clone());
    row.merge_contribution(PendingEntityContribution {
        source_memory_ids: ["memory-2".to_string()].into_iter().collect(),
        ..contribution
    });

    assert_eq!(row.contributions.len(), 1);
    assert_eq!(row.contributions[0].occurrence_count, 2);
    assert_eq!(row.source_memory_ids, ["memory-1", "memory-2"]);
    assert_eq!(row.name, "Postgres");
    assert!(row.search_text.contains("PostgreSQL"));
    assert_eq!(
        row.staged_attributes(),
        serde_json::json!({"version": "17"})
    );
}

#[test]
fn consolidation_stats_reject_incomplete_records() {
    let mut value = serde_json::to_value(ConsolidationStats::default()).unwrap();
    value.as_object_mut().unwrap().remove("resolve_sweeps");

    assert!(serde_json::from_value::<ConsolidationStats>(value).is_err());
}

#[test]
fn legacy_agent_evidence_json_remains_readable() {
    let legacy = serde_json::json!({
        "strength":"explicit",
        "source":{"agent_message":{
            "message_id":"m1","agent_id":"a1","chat_id":"c1","quote":"Acme released 4.2"
        }}
    });
    let evidence: MemoryEvidence = serde_json::from_value(legacy).unwrap();
    assert!(
        matches!(evidence.source, EvidenceSource::AgentMessage { message_id, .. } if message_id == "m1")
    );
}

#[test]
fn portable_memory_roundtrip_preserves_record_links_and_provenance() {
    let original = mem(
        "source",
        Disposition::Outdated,
        &[(RelationType::Replace, "survivor/with-space")],
    );
    let portable: frona_pkm::model::KnowledgeMemory = original.clone().into();
    assert_eq!(portable.relations[0].to, "survivor/with-space");
    let restored: KnowledgeMemory = portable.into();
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}

#[test]
fn portable_entity_roundtrip_preserves_checkpoint_progress() {
    let mut original = KnowledgeConsolidationEntity::pending(
        "run",
        "user",
        "people/casey",
        EntityCategory::Concept,
        Vec::new(),
        Default::default(),
    );
    original.checkpoint_revision = 7;
    original.progress.identity = IdentityProgress::Unresolved {
        fingerprint: "fingerprint".into(),
        diagnostic: serde_json::json!({"reason": "pending"}),
    };
    let portable: frona_pkm::model::KnowledgeConsolidationEntity = original.clone().into();
    let restored: KnowledgeConsolidationEntity = portable.into();
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}
